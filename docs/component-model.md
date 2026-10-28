# ModernWeather — the component and capability model

**Work item:** W2a. **Produces:** this document. **Depends on:** W0 (`docs/DESIGN.md`, `docs/spike-findings.md`).
**Consumed by:** W3 (service architecture), W5 (management UI), W7 (collector).

This document owns what a component **is**: the abstraction, the trigger set, the capability set, the
dependency graph, the lifecycle state machine, and the decomposition of the thing the Go
implementation calls a "driver". It contains trait and enum **sketches** and no implementation code.

It does **not** own pipeline internals, buffer sizes, module or crate layout (W3), the metric registry
(W2), the database schema (W4), or anything user-facing (W5).

---

## Two things to read before the rest

### 1. Proposed amendment to `DESIGN.md` — D-30, flagged not resolved

**D-30** says sinks are composable and "rate-limited upload sinks coalesce to the newest reading."
This document's §4b concludes that rate-limited outputs are **not stream subscribers at all**, so
there is nothing for them to coalesce: APRS, CWOP, Weather Underground and PWS Weather are scheduled
producers that read a *windowed view* of history, because the protocols require averages and peaks
over 2–10 minute windows and a point sample is the wrong number. D-30's *intent* — never backfill a
rate-limited service with an hour of history — is preserved and strengthened. Its *mechanism* is not.

Per the amendment protocol, this document carries on consistently with its own §4b design, and the
proposed amendment is written out in full in **Proposed amendments to `DESIGN.md`** at the end. It is
not applied to `DESIGN.md` here.

Nothing else in `DESIGN.md` is contradicted. D-23 through D-28 are implemented as written, and D-27
is implemented more strictly than it was stated (see §1.1).

### 2. The vocabulary in this document is internal and must not reach a screen

`Component`, `Capability`, `Trigger`, `Transport`, `Codec`, `Session`, `Identity`, `Waiting`,
`FrameSink`, `WindowedView` are names chosen for Rust source. They are **worse** than what exists
today when shown to a person: "Component `aprs:backyard` is Waiting on Capability WindowedView" tells
an operator nothing. **W5 §0 owns the translation table and an internal term on screen is a bug**
(D-36).

What this document owes W5 in exchange is *structure*: every lifecycle state carries the data needed
to render a plain sentence. `Waiting { on }` names the capability and the component that provides it,
so W5 can render "Waiting for the database — nothing will be sent to Weather Underground until it is
back". Every state in §3 is specified with that obligation in mind.

---

## 1. The component abstraction

### 1.1 Declaration is separate from construction, and comes first

The sketch in the W2a prompt puts the declaration on the component:

```rust
trait Component {
    fn trigger(&self)  -> Trigger;
    fn requires(&self) -> &[Capability];
}
```

This cannot work, and the reason is the exact defect D-27 exists to remove. To learn what a component
requires you must have an instance; to have an instance you must call its constructor; and its
constructor is where the Go implementation does this:

```go
// internal/controllers/weather_service_utils.go:38
func NewWeatherServiceController(...) (*WeatherServiceController, error) {
    if err := ValidateTimescaleDBConfig(configProvider, "weather service"); err != nil {
        return nil, err   // PWS, Weather Underground, and the snow cache die here
    }
    db, err := SetupDatabaseConnection(configProvider, logger)
    ...
}
```

Three more constructors do the same (`internal/controllers/aprs/controller.go:67`,
`internal/controllers/aerisweather/controller.go:191`, and the shared base above, which pwsweather
and wunderground both go through). A declaration that lives behind a constructor is a declaration you
can only read once the thing it describes has already succeeded in starting.

**So the model is two-phase, and the first phase is pure.**

```rust
/// Phase 1 — pure. Derived from configuration, no I/O, no sockets, no database.
pub struct ComponentSpec {
    pub id:       ComponentId,          // stable, human-meaningful: "aprs:backyard"
    pub trigger:  Trigger,
    pub requires: Vec<Need>,
    pub provides: Vec<Provision>,
    pub build:    Builder,              // phase 2, not run yet
}

pub struct Need {
    pub capability: Capability,
    pub strength:   Strength,           // Hard | Soft
    pub access:     Access,             // Read | Write
}

pub enum Strength { Hard, Soft }
pub enum Access   { Read, Write }

pub struct Provision {
    pub capability: Capability,
    pub arity:      Arity,              // Exclusive | Additive
}
```

`plan(&Config) -> Result<Vec<ComponentSpec>, PlanError>` is a pure function. It is a unit test, it is
a CI check over the example configurations, and it is where every structural error is caught: an
unknown capability, two `Exclusive` providers of the same capability, a requirement nobody provides, a
cycle, a tick deadline longer than its period, an interval below a hard floor. **None of these can
reach a running process.**

`Builder` is the second phase and runs *per start attempt*, after the component's hard requirements
are available:

```rust
pub enum Builder {
    Tick     (Box<dyn FnOnce(&Cx) -> BoxFuture<'_, Result<Box<dyn Tick>>>      + Send>),
    OnReading(Box<dyn FnOnce(&Cx) -> BoxFuture<'_, Result<Box<dyn OnReading>>> + Send>),
    Serve    (Box<dyn FnOnce(&Cx) -> BoxFuture<'_, Result<Box<dyn Serve>>>     + Send>),
}
```

A builder may fail — a serial port can be missing, a certificate can be unreadable. A build failure is
a lifecycle transition (`Starting → Failed → Backoff → Starting`), not a startup abort. That single
change is what replaces "four components refuse to construct and stay dead until the process is
restarted by hand."

### 1.2 The single-`tick` question: resolved as **no**

The prompt asks whether one `tick` is the right shape for all six triggers, or whether `Service`
needs a different entry point, and notes that a trait which lies about one variant is worse than two
traits. The answer is **three executable traits behind one declarative one**, because there are
exactly three distinct contracts and the difference between them is *what the supervisor must supply
and when it may expect a return*:

```rust
#[async_trait]
pub trait Tick: Send {
    /// Bounded work. MUST return before the declared deadline. Called once per occurrence.
    async fn tick(&mut self, cx: &Cx) -> Result<(), ComponentError>;
}

#[async_trait]
pub trait OnReading: Send {
    /// Bounded work over a batch the supervisor supplies. MUST return before the deadline.
    async fn readings(&mut self, batch: ReadingBatch, cx: &Cx) -> Result<(), ComponentError>;
}

#[async_trait]
pub trait Serve: Send {
    /// Unbounded. Owns its own accept/receive loop. Returns only on cancellation or fatal error.
    async fn serve(self: Box<Self>, cx: &Cx) -> Result<(), ComponentError>;
}
```

A single `tick` would have to mean both "return promptly, I am timing you" and "never return, I am
not timing you". Every piece of supervisor machinery then becomes ambiguous: the overlap policy is
meaningless for a method that never returns; "last run / next run" is meaningless; and a supervisor
that awaits `tick` before scheduling the next occurrence deadlocks the moment it is handed a
listener. The split is not taxonomy for its own sake — `Trigger::Service` is the only variant that
maps to `Serve`, and the mapping is total and statically checked:

| Trigger | Executable trait | Supervisor supplies | Completion contract |
|---|---|---|---|
| `OnReading` | `OnReading` | a `ReadingBatch` | returns before deadline |
| `Every` | `Tick` | nothing | returns before deadline |
| `Calendar` | `Tick` | nothing | returns before deadline |
| `OnStartup` | `Tick` | nothing | returns before deadline, once |
| `OnDemand` | `Tick` | nothing | returns before deadline |
| `Service` | `Serve` | nothing | returns only on cancellation |

`ReadingBatch` rather than a single reading: the storage writer's throughput ceiling in the Go version
is one `INSERT` per reading behind one blocking channel send, and a batch of one is the degenerate
case of a batch. Costing nothing to express here, it is worth expressing here.

`Cx` is the component's context: its resolved capability handles, its cancellation token, its logger
pre-tagged with its `ComponentId`, and its metric handles. It is **not** a service locator — see §2.3.

### 1.3 The supervisor owns every clock

D-25. No component constructs a ticker. Counted in the Go tree, there are **21 separately
hand-rolled scheduling clocks**, excluding reconnect and retry backoffs inside the drivers, which are
legitimately a `Service`'s own business:

| Where | Clocks |
|---|---|
| APRS: reconcile ticker, per-device report ticker, initial-delay timer, 90 s health monitor | 4 |
| `StartPeriodicReports` (PWS + Weather Underground): report ticker + 15-second initial timer | 2 |
| Aeris: forecast refresh, alerts refresh, expired-alert cleanup | 3 |
| Snow cache: depth 30 s, events 15 min, smoothing 15 min, plus a 30-second wait-for-data poll | 4 |
| REST server + weather-apps gRPC: two 3-second live-update poll loops | 2 |
| AirGradient poll (3 s), WeatherLink Live poll, WeatherLink Live broadcast re-arm | 3 |
| Storage health monitors (TimescaleDB 60 s, gRPC 60 s), gRPC connection monitor 30 s | 3 |

Each with its own error handling, none of them inspectable. The tree also contains an unused generic
`RunPeriodicTask(ctx, PeriodicTask{Name, Interval, Task})` helper (`internal/controllers/utils.go:92`)
with **zero callers** — somebody already reached this conclusion and had nowhere to put it.

Moving them to the supervisor buys three things that matter more than the deduplication:

1. **"When did this last run, when does it run next, did it fail" is state**, not a local variable
   inside a goroutine. That is the surface W5 §2 renders.
2. **Phase separation is centrally applied** (§1.5), so the uploaders stop stampeding.
3. **An interval becomes a parse-time constraint** rather than a per-call clamp (§4b).

A `Service` component owns time *inside* its loop — a UDP read deadline, a reconnect backoff, a
keep-alive. That is not a violation of D-25, which is about *scheduling*: a `Service` has no schedule
to own. This is recorded as an assumption for W3 rather than left implicit.

### 1.4 `Trigger`

```rust
pub enum Trigger {
    /// Driven by reading arrival. The supervisor delivers batches; see §5.
    OnReading {
        filter: ReadingFilter,
        policy: ReadingPolicy,
        deadline: Duration,
    },

    /// Fixed period. The supervisor owns the clock.
    Every {
        period:   Duration,
        deadline: Duration,          // <= period, enforced at plan time
        first:    FirstRun,
        phase:    Phase,
    },

    /// Wall-clock schedule in a named time zone. See §1.6.
    Calendar {
        spec:     CronSpec,
        tz:       TzSource,
        misfire:  Misfire,
        deadline: Duration,
    },

    /// Once, when its requirements are first satisfied. Terminal state is Finished.
    OnStartup { deadline: Duration },

    /// Only when asked, via the ComponentControl capability. No schedule.
    OnDemand { deadline: Duration },

    /// Owns its own loop. No schedule, no deadline, no overlap policy.
    Service,
}
```

**Overlap policy: there is exactly one, and it is `Skip`.** The prompt asks for an overlap policy and
answers it correctly for rate-limited outputs — skip and count, never queue, because a queue builds a
backlog that later fires as a burst, which is the thing the rate limit exists to prevent. This
document goes one step further and removes the need for a policy *choice*:

> **Every scheduled component declares a `deadline`, and `deadline <= period` is checked at plan time.
> A tick that exceeds its deadline is cancelled and recorded as `deadline_exceeded`.**

With a deadline no longer than the period, an overlap can only ever be momentary, so `Skip` is the
only sensible policy and a skip becomes a *diagnostic* rather than a routine occurrence. Two counters
are exported per component and surfaced by W5: `skipped_total` and `deadline_exceeded_total`.
Repeated skips mean the configured interval is shorter than the round trip; repeated deadline hits
mean the remote end is hung. Today both conditions are invisible.

The deadline also has to be large enough for the retry budget inside it, which is checked at plan
time — see §4b.

### 1.5 Jitter: deterministic phase, not per-tick randomness

Today every uploader performs its "initial upload in 15 seconds"
(`weather_service_utils.go:131`) and then fires on intervals aligned to that instant, so all of them
hit the local database and their upstream services together, and they do it again in lockstep after
every restart.

The fix is **not** random jitter per tick. Random jitter destroys the predictability of "next run at",
which is one of the three things §1.3 exists to provide, and it buys nothing that phase separation
does not already buy.

```rust
pub enum Phase {
    /// offset = stable_hash(component_id) mod period. Deterministic across restarts.
    Spread,
    /// offset = 0. For components that must align to a wall-clock boundary.
    Aligned,
    Fixed(Duration),
}

pub enum FirstRun {
    /// First occurrence at the next phase-aligned slot — within one period.
    AtPhase,
    /// First occurrence immediately on reaching Running. For warm-up work.
    Immediate,
}
```

`Phase::Spread` is the default. The offset is derived from the component id, so two Weather
Underground uploaders for two stations are separated by a stable, reproducible amount, "next run at"
is answerable, and a restart does not reshuffle the schedule. `FirstRun::Immediate` replaces the
hard-coded 15-second initial timer for the few components that genuinely need to be warm at boot;
everything else starts within one period, which for a 60-second uploader is indistinguishable and for
a 6-hour Aeris refresh is the correct behaviour anyway (today Aeris calls the API immediately on
every process start, because `time.Ticker` only fires after the interval elapses — a restart loop is
a billing event).

### 1.6 `Calendar`: time zones, DST, and missed windows

The measured situation is worse than "`Every` cannot express local midnight". Devices carry a
`timezone` field (`pkg/config/provider.go:193`), and the daily, monthly and yearly rainfall totals —
the things that are supposed to reset at local midnight — **ignore it entirely**:

```sql
-- internal/controllers/rainfall_calc.go:30
WHERE stationname = ? AND bucket >= date_trunc('day', NOW())
```

`date_trunc('day', NOW())` truncates in the *database server's* time zone. The device's declared
time zone is used in exactly two places, both of them rendering sun times for display
(`restserver/handlers.go:373`, `weatherapps_grpc.go:306`). So today the field is decorative for every
rollover that claims to use it.

```rust
pub enum TzSource {
    /// The station's IANA zone. Validated at config parse; a station with a Calendar
    /// component and no valid timezone is a plan error, not a silent fallback to UTC.
    Station(StationId),
    /// The deployment's configured default zone, for components not scoped to a station.
    Deployment,
}
```

**Rules, all of which belong to the supervisor and none of which a component sees:**

- **Zones are IANA names**, resolved through a compiled zone database (`chrono-tz`/`jiff`), validated
  at plan time. `"MST"` and `"-0700"` are rejected: a fixed offset cannot express DST, which is the
  entire reason `Calendar` exists.
- **Every firing has an occurrence key** `(component_id, local_date, slot_index)`. This is what makes
  misfire handling idempotent and "did this occurrence already run" answerable. The supervisor
  persists `(occurrence_key, completed_at, outcome)` for the most recent occurrence per component.
- **Spring forward (a local time that does not exist).** `00:30` on a day where the gap is at
  midnight: fire at **the first instant that exists after the gap**. The occurrence is not skipped,
  because a daily rollover that silently does not happen once a year is the worst possible failure
  mode — it is invisible and it corrupts one day's totals.
- **Fall back (a local time that happens twice).** Fire on the **first** occurrence only. The
  occurrence key is keyed on local date and slot, not on the instant, so the second pass is recognised
  as already completed and skipped. Firing twice would double-count a rollover.
- **Missed window (the process was down across the scheduled instant).** Policy, with a stated
  default:

```rust
pub enum Misfire {
    /// Run the most recent missed occurrence only, if it is within `grace`. Default.
    RunLatest { grace: Duration },
    /// Never make up a missed occurrence; record it and wait for the next.
    Skip,
}
```

`RunLatest { grace: 24h }` is the default because every `Calendar` user in this system is
**idempotent and only-the-latest-matters**: a rollover recomputes a total from stored data, an expiry
sweep deletes what has expired, a snow-season boundary recomputes a season. Running all twelve missed
occurrences after a twelve-hour outage would do eleven units of useless work. `Skip` exists for
anything that must not be made up; nothing in the current system needs it.

A missed occurrence older than `grace` is recorded as `misfired_total` and surfaced, because "this
process has been down longer than a day" is something an operator should see stated rather than infer.

---

## 2. Capabilities and the dependency graph

### 2.1 The set, and why each one is in it

```rust
pub enum Capability {
    // --- roots: provided by the runtime spine before any component exists ---
    ReadingStream,      // the live reading fan-out (W3 owns the internals)
    ComponentControl,   // read component state; start/stop/run-now. Provided by the supervisor.

    // --- provided by config:sqlite ---
    ConfigStore,        // read configuration + subscribe to the change watch
    ConfigAdmin,        // validated configuration mutation
    StationRegistry,    // station identity, coordinates, callsign, secrets, registration

    // --- provided by db:postgres ---
    ReadingStore,       // queryable reading history
    ForecastStore,      // Aeris forecast rows
    AlertStore,         // Aeris alert rows
    SnowCache,          // snow depth / event / smoothing cache rows

    // --- provided by ordinary components ---
    WindowedView,       // (Metric, Window, Agg) resolution over recent aggregate buckets
    LatestSnapshot,     // pre-serialised per-station current conditions
    HttpRoutes,         // additive: route tables mounted by the one listener
}
```

Each is justified by something that exists today, per the prompt's instruction not to invent
capabilities no component needs:

| Capability | Who needs it today, and what goes wrong without it |
|---|---|
| `ReadingStream` | The two components that are actually reading-driven: the TimescaleDB writer and the gRPC uplink. |
| `ComponentControl` | The management API shows controller status and runs `/test/*` checks. Today it reaches this through `AppReloader` — see §2.5. |
| `ConfigStore` | Every component reads configuration; the Go REST controller instead holds it as unsynchronised mutable fields (D-28). |
| `ConfigAdmin` | 46 management routes mutate configuration. |
| `StationRegistry` | APRS is the only Go controller handed `stationManager` (for coordinates and callsign) and `grpcreceiver` bolts on its own `RemoteStationRegistry`. Two mechanisms, one need. |
| `ReadingStore` | The uploaders, the website, the snow calculations. |
| `ForecastStore` / `AlertStore` | `/forecast/24`, `/forecast/240`, `/alerts`. |
| `SnowCache` | `/snow`, the second hot path. |
| `WindowedView` | §4b. Today APRS runs `SELECT calculate_wind_gust(?)` from inside the radio component, with a comment saying "same as website" — one concept, two call sites, no owner. |
| `LatestSnapshot` | `/latest` is ~93% of traffic at 7 round trips per request. W3 §2a owns the mechanics; this document owns the edge. |
| `HttpRoutes` | Seven listener-owning sites today (REST HTTP/HTTPS, REST gRPC, management, grpcreceiver, grpcstream, plus one per Ambient device and one per WeatherLink Live device). |

Two capabilities named in the prompt's starting sketch are **deliberately absent**:

- **`SunTimes` is not a capability**, because the sun-times precompute is not a component. See §4.3.
- **A `Reconfigure` capability is not needed.** The prompt suggests reconfiguration "should be a
  capability backed by the config watch channel". It should be a *root fact*, not a capability: every
  component's `Cx` carries the config watch receiver unconditionally, because every component needs it
  and a dependency that everything declares is noise in the graph. The *mutating* side is
  `ConfigAdmin`, which exactly one component requires. See §2.5.

### 2.2 Arity, strength, and what the graph is actually for

The graph exists to answer two questions and no others: **in what order do components start**, and
**is this component `Waiting`**. Both are questions about *availability*. Everything else people
expect a dependency graph to carry is a different concern wearing the same hat, and conflating them
is how the current relationships ended up unexpressed.

**Arity.**

- `Exclusive` — exactly one provider. Two `Exclusive` providers of the same capability is a plan
  error. This covers everything except routes.
- `Additive` — many providers, one consumer, and **late-bindable**: the consumer starts with whatever
  is published and accepts more as providers become ready. `HttpRoutes` is the only additive
  capability.

The late-binding rule for `HttpRoutes` is load-bearing and deserves to be a decision rather than a
detail: **the listener must not wait for all route providers.** If it did, a management UI would be
unreachable precisely when something is broken — which is precisely when you need it. So the listener
comes up with the routes it has, and a request to a path whose provider is not yet `Running` returns
`503` with a body naming the component and the capability it is waiting on. That is a better answer
than a 404 and it is the same information W5 renders.

**Strength.**

- `Hard` — the component does not start without it, and returns to `Waiting` if it is revoked.
- `Soft` — the component starts regardless. The handle resolves to `Unavailable` and the component
  degrades in a defined way.

`Soft` is what keeps the website from waiting on Aeris. The public site requires `LatestSnapshot`
hard and `ForecastStore`, `AlertStore` and `SnowCache` soft, so a failed Aeris component costs you
`/forecast` and nothing else. **Only `Hard` edges form the ordering graph**, which also makes `Soft`
the escape hatch if a legitimate cycle ever appears.

**What the graph is not for: data presence.** `/forecast` depends on Aeris having fetched at least
once. That is not a startup edge — modelling it as one would mean the website does not serve until
Aeris has succeeded, which is worse than today. It is a **freshness** property, declared separately
and consumed by W5 and by `/test/*`, never by the scheduler:

```rust
pub struct Freshness {
    pub capability:      Capability,
    pub populated_by:    ComponentId,
    pub expected_max_age: Duration,
}
```

So the distinction this document insists on is: **capability availability is a lifecycle concern; data
presence is a response concern.** `LatestSnapshot` is available the instant the snapshot component is
`Running`, and its contents may be empty because no reading has arrived yet. An empty snapshot is a
defined response that W1 must pin, not a reason to hold the listener down.

### 2.3 How a capability reaches a component: typed handles, resolved once

**Typed handles, resolved in `build`, not a registry lookup at the point of use.**

A registry lookup at use time (`cx.get::<ReadingStore>()?` inside a tick) means every call site can
fail, so "the database is missing" becomes an error branch in business logic instead of a lifecycle
state — which is exactly the shape being removed. Resolving once means that *after* `build`, a
component cannot observe absence at all:

```rust
// What a component's build closure receives, and what it keeps.
struct AprsHandles {
    windows:  WindowedViewHandle,          // Hard
    stations: StationRegistryHandle,       // Hard
}
```

A `Soft` requirement resolves to a handle that is explicit about unavailability
(`Option<Handle>`, or a handle whose methods return `Err(Unavailable)`), so degradation is written
down in the one component that degrades rather than handled everywhere.

Handles are published through a watch per capability, starting at `Unavailable` and moving to
`Ready(handle)` when the provider signals readiness. **Readiness is explicit and distinct from
`Running`**, because `db:postgres` is `Running` from the moment its task starts and usable only after
a probe succeeds. Publication through a watch is also what makes revocation possible, which is what
makes `Waiting` re-enterable (§3).

Handles are cheap to clone and carry no exclusive ownership — `arc-swap` / `tokio::sync::watch` per
D-32.

### 2.4 Resolution, ordering, and cycles

```
plan(config)
  → specs: Vec<ComponentSpec>                      (pure)
  → validate arity, floors, deadlines, identities  (pure)
  → build hard-edge digraph: requires(Hard) → provides
  → toposort                                        (pure)
  → Err(PlanError::Cycle { path }) on failure
```

**Cycles are detected at plan time and are fatal there, before any component is constructed.** Not at
startup, and not at build time in the Rust sense — the graph is a function of configuration, so it
cannot be settled when the binary is compiled, but it *can* be settled without touching a socket.
That matters: it makes cycle detection a unit test over fixture configurations and a CI check over
the shipped examples, and it means a cycle is reported as a config error naming the path rather than
as a hang.

The topological order gives the **start** order. Shutdown walks it in reverse. Within a rank,
components start concurrently; there is no reason to serialise siblings.

The ordering is advisory rather than load-bearing, which is a deliberate property: because a `Hard`
requirement means `Waiting` rather than failure, a component started "too early" simply waits. The
topological order is an optimisation that avoids a flurry of `Waiting` transitions at boot, not a
correctness requirement. A supervisor that started everything in random order would converge to the
same state — which is the property that makes this robust to the one case the Go version cannot
survive, a dependency that goes away and comes back.

### 2.5 The `AppReloader` cycle is gone

The cycle, stated precisely as it exists:

```
app ──constructs──▶ controllerManager ──constructs──▶ every controller
 │                        │
 │                        └── explicitly skips type == "management"   (managers/controller.go:50)
 └──constructs──▶ management.Controller(app: interfaces.AppReloader)  (management/controller.go:34)
                                │
                                └── calls app.ReloadConfiguration / AddController /
                                    RemoveController / AddWeatherStation / RemoveWeatherStation
```

The management controller depends on the application object that owns the manager that constructs
every other controller. It is a genuine cycle, and the price paid for it is that the management
controller is special-cased out of the controller manager entirely — `if con.Type == "management" {
continue }` — so the one component an operator interacts with is the one component with no lifecycle
management at all.

**It is eliminated by splitting the back-reference into two one-way capabilities, both provided by
things that are not components.**

```
config:sqlite ──provides──▶ ConfigStore (read + watch), ConfigAdmin (mutate), StationRegistry
supervisor    ──provides──▶ ComponentControl (observe state; start / stop / run-now)

api:management ──requires──▶ ConfigAdmin (Hard, Write), ComponentControl (Hard), HttpRoutes (provides)
```

Walk the former `AppReloader` methods through it:

| `AppReloader` method | Replacement | Direction |
|---|---|---|
| `ReloadConfiguration(ctx)` | **nothing calls it.** A write through `ConfigAdmin` bumps the config watch; the supervisor is a watch subscriber and reconciles the plan itself. | one-way |
| `AddController(cfg)` | a `ConfigAdmin` write. The supervisor re-plans, diffs specs, and starts the new one. | one-way |
| `RemoveController(type)` | a `ConfigAdmin` write. The supervisor cancels the component and awaits its task. | one-way |
| `AddWeatherStation(name)` | a `ConfigAdmin` write — stations are components (§4a), so this is the same path. | one-way |
| `RemoveWeatherStation(name)` | same. | one-way |

The management API no longer reaches back into the application; it writes configuration and observes
component state. **The supervisor is the only thing that acts on a configuration change**, and it is a
root — it exists before any component and requires nothing, so it cannot participate in a cycle. The
management API becomes an ordinary leaf component with an ordinary lifecycle, which is the part that
actually fixes the operational problem: today `RemoveController` can only delete a map entry and log
that the controller will stop at the next application restart, while its listeners and goroutines
leak (D-24).

One consequence worth stating because W5 depends on it: **reconfiguration is a plan diff, not a
restart.** The supervisor re-runs `plan()` on the new configuration, diffs by `ComponentId` and spec
equality, and then stops removed components, starts added ones, and restarts changed ones. Unchanged
components are not touched — so saving a website configuration no longer disturbs the APRS schedule.
And because `plan()` is pure, an invalid configuration is rejected *before* anything is stopped. The
current behaviour is the opposite: the Go REST controller replaces its website list, device list,
device-name map, devices-by-website map and TLS configuration wholesale, as plain fields with no
mutex, while in-flight requests read them — a concurrent map read and write in Go is an unrecoverable
fatal error, so saving a website while serving traffic can kill the process (D-28).

### 2.6 `LatestSnapshot` and `HttpRoutes` in the graph

**`LatestSnapshot`.** Provided by `snapshot:latest`, a `Trigger::OnReading` component with
`ReadingPolicy::Latest`. Required `Hard` by `api:legacy` and by `web:site:*`. W3 §2a owns the
mechanics — what is in the snapshot, how it is serialised, how derived values get there. What this
document fixes is its place in the graph and one invariant:

> **The `/latest` handler performs no queries.** Not fewer queries — none. The capability is a
> pre-serialised payload, so if the handler can reach a database handle at all, the design has
> leaked.

The handler therefore requires `LatestSnapshot` and **does not require `ReadingStore`**. That is the
whole point of declaring access per capability (§2.1): the thing that must not query cannot, because
it was never handed a handle that can.

**`HttpRoutes`.** Additive, late-bindable, one consumer. Route-table providers, today each owning its
own listener:

| Provider | Routes | Listener today |
|---|---|---|
| `web:site:<id>` | the public weather website, per hostname, per-site TLS via SNI | shared with `api:legacy` |
| `api:legacy` | the frozen REST surface (W1) | shared |
| `api:management` | 46 management routes | its own `http.Server` |
| `grpc:weatherv1` | gRPC `WeatherV1` | its own listener |
| `grpc:weatherappsv1` | gRPC `WeatherAppsV1` | shared with `WeatherV1` |
| `station:ambient:<id>` | `/ingest/ambient` | **one `http.Server` per device** |
| `station:grpcrx` | inbound gRPC readings | its own listener |
| `uplink:*` server side | gRPC stream receive (W7) | its own listener |

One `http:listener` component, `Trigger::Service`, mounts all of them — `axum` and `tonic` over one
port per D-32, gRPC as a codec and not a kind of component per D-31. Multi-hostname routing and
per-website SNI stay as behaviour and become listener configuration; W3 §6 owns how.

---

## 3. Component lifecycle

### 3.1 States

```
                        ┌────────────── capability revoked ──────────────┐
                        ▼                                                │
  Planned ──▶ Waiting { on } ──▶ Starting ──▶ Running { degraded } ──────┤
                   ▲                 │              │                    │
                   │                 │              ├── Ok, trigger done ─▶ Finished
                   │                 │              │                    │
                   └── Backoff ◀─────┴──────────────┴── Err ──▶ Failed ──▶ Backoff
                                                     │
                 cancel ─────────────────────────────┴──────▶ Stopping ──▶ Stopped
```

| State | Carries | Meaning |
|---|---|---|
| `Planned` | — | In the plan, not yet considered. |
| `Waiting { on: Vec<(Capability, ComponentId)> }` | the capabilities and **who provides each** | A hard requirement is unavailable. Nothing has been constructed. |
| `Starting` | attempt number | The builder is running. |
| `Running { degraded: Vec<Capability> }` | which soft requirements are unavailable | Live. `degraded` is non-empty when a soft requirement is missing. |
| `Failed { error, at, attempt }` | the typed error with its full cause chain (D-34) | The last attempt or tick failed. |
| `Backoff { until, attempt }` | when it will retry | Waiting out a delay. |
| `Finished { at }` | — | `OnStartup` completed, or a removed component stopped cleanly. Terminal. |
| `Stopping { since }` | — | Cancelled, awaiting its task. |
| `Stopped` | — | Terminal. |

`Waiting` carrying the *provider* and not just the capability is what lets W5 render a sentence a
person can act on. "Waiting on ReadingStore" is internal jargon; "waiting for the database
(`db:postgres`), which is retrying in 12 s" is an operator-facing sentence, and the second one is
reachable from the first only if the state carries the edge.

`degraded` on `Running` rather than a separate `Degraded` state: a degraded component is running and
serving, and a state machine that distinguishes them forces every consumer to handle both. The
website with Aeris down is `Running { degraded: [ForecastStore, AlertStore] }`.

### 3.2 Who drives transitions

**The supervisor, exclusively.** A component never sets its own state. It reports an outcome, by one
of exactly three means: returning `Ok` from `tick`/`readings`, returning `Err`, or being cancelled.
There is no `set_state`, no health method, no `IsRunning`.

This deletes a category of code. APRS invented `Start`, `Stop`, `IsRunning` and `GetHealth`
(`aprs/controller.go:133-191`) because it needed them, but the `Controller` interface declares only
`StartController()`, so none of them is reachable through the interface and `Stop()` is dead code. A
second health path exists in parallel through a package-level `storage.GlobalHealthManager`, updated
by a 90-second ticker inside the APRS controller. **Component state is the health surface**; there is
no second mechanism.

The one genuinely useful thing the APRS health monitor did — can we authenticate to APRS-IS — is not
health, it is a connectivity check. It becomes an `OnDemand` component alongside the other six `/test/*`
checks (W5 §1), where it belongs: run when a human asks, not every 90 seconds forever.

### 3.3 Backoff

The scheduled triggers already have a retry clock, and adding a second one is how you get two
competing schedules.

- **`Every` / `Calendar`: the period *is* the backoff.** A failed tick is recorded
  (`failed_total`, `last_error`) and the next scheduled occurrence is the retry. No separate delay, no
  drift, no doubling of the interval behind the operator's back — a 60-second uploader that is failing
  retries every 60 seconds, which is both what the operator configured and what the remote service's
  rate limit expects.
- **`Starting` failures and `Service` exits: exponential backoff with full jitter**, base 1 s, cap
  60 s, reset after a run that lasted longer than the cap. This covers a missing serial port, an
  unreadable certificate, a listener whose port is in use, a dropped TCP session.
- **No permanent give-up.** The cap is 60 s and it keeps trying. "Dead until a human restarts the
  process" is the defect being removed; replacing it with "dead after N attempts" reintroduces it with
  extra steps. What makes this acceptable rather than a silent infinite loop is that
  `restart_total`, `attempt`, `last_error` and `Backoff { until }` are all state (§3.1) — a component
  restarting every minute for a day is *visible*, which is the thing that was missing, not the
  retrying.

### 3.4 What `Failed` does to dependents: nothing, by default

**A `Failed` component does not cascade.** Dependents keep their last good handle and keep running.

A dependent moves `Running → Waiting` only when the capability is **revoked**, and only the provider
revokes, and only when the capability is genuinely unusable — a dead connection pool, not a single
failed query. One slow query must not be able to take down the system, and D-27's whole premise is
that unavailability is waiting rather than death.

Worked example, because the asymmetry is the valuable part. The database becomes unreachable:

| Component | Today | Under this model |
|---|---|---|
| `db:postgres` | — | probe fails → revokes `ReadingStore`, `ForecastStore`, `AlertStore`, `SnowCache` → `Failed` → `Backoff` |
| `writer:timescale` | engine goroutine errors per reading; the unbuffered fan-out send blocks and back-pressures the serial read loops | `Waiting { on: ReadingStore }`; the stream does not block (§5) |
| `window:rollup` | — | `Waiting { on: ReadingStore }` |
| `aprs:*`, `pws:*`, `wu:*` | **dead until the process is restarted** (constructor validation) | `Waiting { on: WindowedView }`, resume automatically |
| `snapshot:latest` | — | **unaffected** — it folds arrivals in memory |
| `api:legacy` `/latest` | 7 failing round trips per request | **unaffected**, 93% of traffic still served |
| `api:legacy` `/forecast` | failing query | `Running { degraded: [ForecastStore] }`, defined stale/empty response |
| `api:management` | unaffected | unaffected — and still reachable, by the late-binding rule in §2.2 |

The row that matters is the last-but-two: a database outage currently takes out the hottest endpoint
in the system, and under this model it does not touch it.

### 3.5 Stopping is cancellation plus ownership

D-24. There is no stop method anywhere in the abstraction.

```
supervisor cancels the component's CancellationToken
  → the task observes cancellation at its next await point
  → Drop runs: sockets closed, serial port released, pool returned, spool flushed
  → supervisor awaits the JoinHandle with a grace deadline
  → deadline exceeded → abort() the task, record stop_timeout_total, log with component id
```

Full shutdown walks the topological order in reverse, so a provider outlives its consumers and a
consumer never observes a revoked capability during shutdown. Resource release is `Drop`, not a
method, which is what makes "removing a controller leaks its listeners" structurally impossible
rather than merely fixed.

---

## 4. Every existing component, mapped

Trigger / requires / provides / what the body does, for all eleven long-running components in the W2a
prompt's table, plus the seven station drivers and both storage writers. `H` = Hard, `S` = Soft,
`W` = write access.

### 4.1 Ingest and storage

| New component | Go origin | Trigger | Requires | Provides | Body |
|---|---|---|---|---|---|
| `db:postgres` | `internal/database`, `storage/timescaledb` | `Service` | — | `ReadingStore`, `ForecastStore`, `AlertStore`, `SnowCache` | Owns the pool; probes; publishes handles on success, revokes on sustained failure. |
| `config:sqlite` | `pkg/config` | `Service` | — | `ConfigStore`, `ConfigAdmin`, `StationRegistry` | Owns the SQLite handle and the change watch. |
| `writer:timescale` | `storage/timescaledb` | `OnReading { All, Durable, 5s }` | `ReadingStore` H W | — | Batch-insert the envelope batch. |
| `uplink:collector:<peer>` | `storage/grpcstream` | `OnReading { NotFrom(self), Durable, 10s }` | spool (W7 §3) | — | Forward to the peer; W7 owns provenance, ack and resume. |
| `snapshot:latest` | `restserver/handlers.go` `/latest` | `OnReading { All, Latest, 100ms }` | `StationRegistry` H, `WindowedView` S, `SnowCache` S | `LatestSnapshot` | Fold the batch into the per-station snapshot, re-serialise, publish. W3 §2a owns the contents. |
| `window:rollup` | `calculate_wind_gust()`, `rainfall_calc.go` | `Every { 60s, 10s, Immediate, Aligned }` | `ReadingStore` H | `WindowedView` | Refresh a bounded in-memory ring of recent 1-minute aggregate buckets per station; resolve `(Metric, Window, Agg)` requests from it (§4b). |

`db:postgres` split from `writer:timescale` is deliberate and is the structural half of the D-27 fix:
pool availability is then independent of write throughput, so what the uploaders wait on is "is the
database reachable", not "is the writer keeping up".

`Phase::Aligned` on `window:rollup` because its input is the 1-minute aggregate, which materialises on
minute boundaries; spreading its phase would read a bucket that is not there yet.

### 4.1b Station components

The seven drivers, as components. §4a decomposes their internals; this is their place in the graph.
All of them resolve identity before emitting (§4a.6), so all of them require `StationRegistry`.

| New component | Trigger | Requires | Provides | Body |
|---|---|---|---|---|
| `station:davis:<id>` | `Service` | `StationRegistry` H | — | Wake + `LOOP n` session; decode packets; emit envelopes. |
| `station:campbell:<id>` | `Service` | `StationRegistry` H | — | Command/response session; decode; emit. |
| `station:snowgauge:<id>` | `Service` | `StationRegistry` H | — | gRPC client stream; decode; emit. |
| `station:airgradient:<id>` | `Every { 3s, 2s }` | `StationRegistry` H | — | One HTTP GET; decode; emit. |
| `station:ambient` | `Service` | `StationRegistry` H | `HttpRoutes` | **One** component for N stations; identity from `?ID=`. |
| `station:wll:<id>` (broadcast) | `Service` | `StationRegistry` H | — | UDP listener; identity from the DSN in the packet. |
| `station:wll:<id>:rearm` | `Every { 0.8 × duration }` | `ConfigStore` H | — | Outbound control call that arms the device's broadcast. Emits no readings. |
| `station:wll:<id>` (poll) | `Every { ≥30s }` | `StationRegistry` H | — | One HTTP GET of `current_conditions`; same codec as broadcast. |
| `station:grpcrx` | `Service` | `StationRegistry` H W | `HttpRoutes` | **One** component for N peers; identity from registration; mTLS. W7 §2 owns provenance. |

Two shapes to note. `station:ambient` and `station:grpcrx` are **one component serving N stations**,
which is the §4a.4 cardinality rule showing up in the plan: the component is per *endpoint*, not per
station, so adding a station does not add a component. Everything with `StationIdentity::Fixed` is one
component per station.

`StationRegistry` is `Write` for `station:grpcrx` alone, because registration mints keys. No other
station component can write to the registry.

### 4.2 Rate-limited outputs

| New component | Go origin | Trigger | Requires | Provides | Body |
|---|---|---|---|---|---|
| `aprs:<device>` | `controllers/aprs` | `Every { ≥floor, ≤period, AtPhase, Spread }` | `WindowedView` H, `StationRegistry` H | — | Resolve the declared `(Metric, Window, Agg)` table; encode the APRS information field; hand to the device's `FrameSink`. **No SQL.** |
| `pws:<device>` | `controllers/pwsweather` | `Every { ≥60s }` | `WindowedView` H | — | Resolve, encode query parameters, one HTTP GET through the tower stack. |
| `wu:<device>` | `controllers/wunderground` | `Every { ≥60s }` | `WindowedView` H | — | Identical shape to `pws`, which is the point — `sendPeriodicReports` is line-for-line identical today. |

One component instance per device, not one controller managing N workers. That deletes
`reconcileWorkers`, its `map[string]context.CancelFunc`, the private `sync.WaitGroup`, the 60-second
reconcile ticker, and the five-level `sendReports → reconcileWorkers → sendDeviceReports →
sendReportForDevice → sendStationReading` chain — all of which exists to do what the supervisor does
for every component anyway. Per-device interval becomes that instance's trigger, and reconciliation
becomes the plan diff in §2.5.

### 4.3 Producers and precomputes

| New component | Go origin | Trigger | Requires | Provides | Body |
|---|---|---|---|---|---|
| `aeris:forecast:<device>:<span>` | `aerisweather`, `refreshForecastPeriodically` | `Every { span/4, ≥1h }` | `ForecastStore` H W, `ConfigStore` H | — | One Aeris API call; upsert the forecast row. |
| `aeris:alerts:<device>` | `aerisweather`, `refreshAlertsPeriodically` | `Every { 15m }` | `AlertStore` H W, `ConfigStore` H | — | One Aeris alerts call; upsert alert rows. |
| `aeris:alerts:gc` | `aerisweather`, `cleanupExpiredAlerts` | `Calendar { daily 03:00, Deployment }` | `AlertStore` H W | — | Delete alerts that expired more than 24 h ago. |
| `snow:depth` | `snowcache`, 30 s ticker | `Every { 30s }` | `SnowCache` H W, `ReadingStore` H, `StationRegistry` H | — | Refresh depth totals per snow station. |
| `snow:events` | `snowcache`, 15 min ticker | `Every { 15m }` | same | — | Cache accumulation events for visualisation. |
| `snow:smoothing` | `snowcache`, 15 min ticker | `Every { 15m }` | same | — | Update smoothed depth estimates. |
| `snow:season` | `calculate_total_season_snowfall()` | `Calendar { daily 00:10, Station }` | `SnowCache` H W, `ReadingStore` H | — | Recompute the season total, which currently scans months on every `/snow` request. |

**Aeris: separate components, not one component with multiple triggers.** The rule is **one component,
one trigger**, and Aeris is the case that proves it. Measured, the Go Aeris controller runs four
independent goroutine families per process: forecast refresh for the 10×24h span, forecast refresh for
the 24×1h span, alerts on a 15-minute ticker, and a daily expired-alert sweep. They have different
periods, hit different endpoints, fail independently, and want independent rate budgets. A single
component with a `Vec<Trigger>` would have to carry per-trigger last-run / skip / deadline state,
which turns every piece of §1.4's accounting into a map keyed by trigger index — and it would make
"alerts are failing, forecasts are fine" inexpressible, which today it is. Four components cost four
rows in a table and buy four independent lifecycles.

`aeris:alerts:gc` is a retention sweep with no API call and only incidentally lives in Aeris; W4 may
reasonably absorb it into a database retention policy. Either home is fine. It is listed here so it is
not lost in the move.

**Snow cache: three components, and its hand-rolled wait loop disappears.** The Go snow cache opens
with this:

```go
// internal/controllers/snowcache/controller.go:88
for !dataAvailable {
    select {
    case <-time.After(30 * time.Second):
        if c.hasRecentSnowData() { dataAvailable = true }
    ...
```

That is a component hand-rolling a wait-for-availability loop, which is the thing §3 generalises. But
note *which* generalisation: "is a snow station producing data" is **data presence, not capability
availability** (§2.2), so this is not a `Waiting` state. The loop vanishes for a simpler reason — the
supervisor is already ticking the component every 30 seconds, so the tick returns early when there is
nothing to do and the loop is pure duplication of the scheduler. The component gets a `last run`,
`next run` and `last error` it does not have today, and the 30-second poll exists once, in the
supervisor, instead of once per component that needed to wait for something.

**Sun-times precompute: it is not a component, and the prompt's premise does not hold.**

The prompt asks for the "real trigger", offering `Calendar` daily extension or `OnStartup` plus a
scheduled top-up, on the grounds that "a precompute has a finite horizon and a process that runs for
months outlives it." Measured, that is not what the code does:

```go
// pkg/config/provider_sqlite.go:2330
for day := 1; day <= 366; day++ {
    sunrise, sunset, err := calculator(day, latitude, longitude)
    ...INSERT INTO sun_times (station_name, day_of_year, sunrise_minutes, sunset_minutes)
```

The table is keyed by **day-of-year, 1–366, with no year**, and is read back as
`GetSunTimes(stationName, dayOfYear)`. There is no horizon to exhaust, so neither offered trigger is
the right answer — the precompute does not expire, it is merely *year-agnostic*, which is a
correctness defect rather than a scheduling one (day-of-year 60 is 29 February in a leap year and 1
March otherwise).

The honest conclusion is that this is not scheduled work at all. Sunrise and sunset are a **pure
function of (date, latitude, longitude)** — Meeus, already implemented in `pkg/solar` — costing
microseconds. Evaluating it where it is needed is *cheaper* than the SQLite lookup it replaces, on the
endpoint that is 93% of traffic. So:

> **`SunTimes` is not a capability, the precompute is not a component, and the `sun_times` table is
> not carried forward.** Sunrise/sunset is evaluated as a pure function at the point of use — in
> practice inside `snapshot:latest`, since that is what serves it. W3 §2a owns where.

Deleting it removes a component, a capability, a table, a startup dependency, a 366-row write per
station per configuration change, and a latent leap-year bug. **The risk, stated:** W1's frozen
contract fixtures must show that a fresh Meeus evaluation reproduces today's stored values to the
minute. If they diverge — rounding, or a day-of-year offset baked into a client's expectations — the
fallback is to reinstate it as `Calendar { yearly, Jan 1, Station }` plus a reactive rebuild on a
coordinate change, and the shape for doing so is specified in §1.6. The fallback is one table row in
the plan; carrying the component forward speculatively is not.

### 4.4 Services

| New component | Go origin | Trigger | Requires | Provides | Body |
|---|---|---|---|---|---|
| `http:listener` | `restserver`, `management`, per-device servers | `Service` | `HttpRoutes` Additive | — | One listener. Multi-hostname routing, per-site TLS via SNI, axum + tonic (D-31, D-32). W3 §6 owns how. |
| `web:site:<id>` | `restserver` website routes | `Service` | `LatestSnapshot` H, `ReadingStore` S, `ForecastStore` S, `AlertStore` S, `SnowCache` S | `HttpRoutes` | Route table for one website. |
| `api:legacy` | `restserver/handlers.go` | `Service` | `LatestSnapshot` H, `ReadingStore` S, `ForecastStore` S, `AlertStore` S, `SnowCache` S | `HttpRoutes` | The frozen REST surface (W1). |
| `grpc:weatherv1` | `restserver/controller.go` gRPC | `Service` | `LatestSnapshot` H, `ReadingStore` S | `HttpRoutes` | gRPC service as a codec over the shared listener. |
| `grpc:weatherappsv1` | `restserver/weatherapps_grpc.go` | `Service` | `LatestSnapshot` H, `ReadingStore` S | `HttpRoutes` | Same. |
| `live:stream` | the two 3-second poll loops | `OnReading { All, Latest, 50ms }` | — | `HttpRoutes` (SSE) | Push on arrival instead of polling. |
| `api:management` | `controllers/management` | `Service` | `ConfigAdmin` H W, `ComponentControl` H, `StationRegistry` H | `HttpRoutes` | 46 routes. **No `AppReloader`** (§2.5). |
| `check:<name>` ×7 | `management/handlers_testing.go`, APRS health monitor | `OnDemand` | varies per check | — | `/test/*` connectivity checks, including "can we authenticate to APRS-IS". |

`live:stream` is worth its row: `restserver/controller.go:1056` and
`restserver/weatherapps_grpc.go:128` each run a `time.NewTicker(3 * time.Second)` that polls for new
data to push to a browser or app. Both become one `OnReading` subscription with
`ReadingPolicy::Latest` — two polling loops and their database traffic deleted, and the browser gets
the reading when it arrives rather than up to three seconds later.

### 4.5 Nothing in the eleven is a misfit, but two things in the *prompt* are

Per the instruction to say so rather than bend the component:

1. **The sun-times precompute does not fit any trigger**, because it is not periodic work. §4.3.
   The model is not wrong; the premise that it is a scheduled precompute is.
2. **"Sinks on a reading stream" does not describe the rate-limited outputs** — §4b and the D-30
   amendment. Of the eleven, exactly two are reading-driven, and the new model adds three more
   (`snapshot:latest`, `live:stream`, and W7's uplink), for five out of roughly thirty. `OnReading` is
   one trigger among six, as the prompt says, and the table above is the evidence.

One further misfit, in §4a rather than §4, is the "driver" decomposition: the three-part split is
missing a part. §4a.2.

---

## 4a. Decomposing "driver"

### 4a.1 The measured situation

| Driver | Who initiates | Transport | Stations per endpoint | Identity today |
|---|---|---|---|---|
| Davis | we do | serial *or* TCP | 1 | config |
| Campbell | we do | serial *or* TCP | 1 | config |
| Snow gauge | we do | gRPC client | 1 | config |
| AirGradient | we do | HTTP poll | 1 | config |
| Ambient | **they do** | HTTP server | 1 — *should be N* | **the port the request arrived on** |
| WeatherLink Live | **both** | UDP listener **+** HTTP | 1 | config |
| gRPC receiver | **they do** | gRPC server | **N** | a bolted-on registry |

Ambient is paying for cardinality not being modelled, exactly as the prompt states. Each device gets
its own server:

```go
// internal/weatherstations/ambientcustomized/station.go:76
s.server = &http.Server{
    Addr: fmt.Sprintf("%s:%s", listenAddr, s.config.Port),
    ...
}
// and the station is identified positionally, by which server received the request:
reading := types.Reading{ StationName: s.config.Name, ... }   // :138
```

Two Ambient stations means two HTTP servers on two ports, and the station's identity is "whichever
listener got the request" — even though the request carries `?ID=` and `?PASSWORD=`, both of which are
read and then used only to reject empty values. The gRPC receiver hit the same wall and solved it
differently, by hanging a `RemoteStationRegistry` off a single device config. Two mechanisms, one
problem, neither of them the model.

### 4a.2 The split is four parts, not three

The prompt proposes:

```
Transport ──frames──▶ Codec ──(StationKey, Reading)──▶ Identity ──▶ Envelope ──▶ stream
```

It is right that `Codec` must be pure, and that purity is the payoff — a pure codec is tested against
captured frames with no hardware. But the pipeline as drawn cannot express Davis, and Davis is the
oldest driver in the tree. Before a Davis console emits a LOOP packet you must wake it (newline,
await `\n\r`, retry) and send `LOOP n`, re-issuing after the run of `n` packets ends. That is
request/response conversation over the transport, it is impure by nature, and if it goes in the codec
the codec stops being pure — which is the one property worth protecting.

So there is a fourth part:

```
Transport ──bytes──▶ Session ──frames──▶ Codec ──(StationKey, Reading)──▶ Identity ──▶ Envelope ──▶ stream
            (impure: handshake,  (pure: bytes → readings)   (resolve + authenticate)
             request/response,
             protocol state)
```

```rust
/// Impure. Owns protocol-level conversation and connection state. Testable against a
/// recorded transcript (bytes in, bytes expected out) with no hardware.
#[async_trait]
pub trait Session: Send {
    async fn open(&mut self, io: &mut dyn ByteStream) -> Result<(), SessionError>;
    async fn next_frame(&mut self, io: &mut dyn ByteStream) -> Result<Frame, SessionError>;
}

/// Pure. No I/O, no clock, no allocation of connections. Golden-testable.
pub trait Codec {
    fn decode(&self, frame: &Frame) -> Result<Vec<PartialReading>, CodecError>;
}
```

`Session` is not invented for a hypothetical. Four existing drivers need it: Davis (wake + `LOOP n`),
Campbell (command/response over serial), WeatherLink Live in broadcast mode (the HTTP call that arms
the broadcast), and the snow gauge (gRPC stream establishment). A fifth, hypothetical one — Modbus —
needs exactly the same thing, which is the test of whether the abstraction is right rather than
convenient.

Drivers that need no session (`Session::None`, an identity session) are the inbound ones: Ambient,
the gRPC receiver, WLL UDP packet decode.

### 4a.3 `Transport`, and the `HttpPoll` question answered

**`HttpPoll` is not a transport.** The prompt says either answer is defensible and asks for the call.
The call is that it is `Trigger::Every` wrapped around an HTTP request, for a reason that is not a
matter of taste: a transport carrying an `every: Duration` puts a timer back inside a component, and
D-25 says the supervisor owns every timer. `HttpPoll { url, every }` would be the one place in the
design where a component schedules itself.

```rust
pub enum Transport {
    // --- we initiate ---
    Serial  { dev: PathBuf, baud: u32 },
    TcpOut  { host: String, port: u16, tls: Option<TlsConfig> },
    /// One request per tick. Trigger::Every owns the cadence, not this.
    HttpOut { url: Url, method: Method, auth: Option<SecretString> },
    GrpcOut { endpoint: Url, streaming: bool },
    MqttSub { broker: Url, topic: TopicFilter },        // §4a.7

    // --- they initiate ---
    HttpIn  { mount: RoutePath },                       // provides HttpRoutes
    UdpIn   { port: u16, group: Option<IpAddr> },
    GrpcIn  { mount: RoutePath },                       // provides HttpRoutes
}
```

Each variant implies its trigger, and the implication is checked at plan time:

| Transport | Trigger | Why |
|---|---|---|
| `Serial`, `TcpOut`, `GrpcOut { streaming: true }`, `MqttSub` | `Service` | A long-lived stream; the component owns a receive loop. |
| `HttpOut`, `GrpcOut { streaming: false }` | `Every` | One request/response per occurrence. |
| `HttpIn`, `GrpcIn`, `UdpIn` | `Service` | A listener. `HttpIn`/`GrpcIn` additionally provide `HttpRoutes`. |

And the convenience constructor the prompt asks for, so the abstraction does not tax Davis:

```rust
// The common case: one station on one wire.
Station::serial("backyard", "/dev/ttyUSB0", 19_200, DavisCodec)
// expands to Transport::Serial + DavisSession + DavisCodec + StationIdentity::Fixed("backyard")
```

### 4a.4 `StationIdentity`, and the invariant that MQTT broke

```rust
pub enum StationIdentity {
    /// Configuration says which station. Exactly one station per endpoint.
    Fixed(StationId),
    /// /ingest/ambient/{station_key}
    FromPath,
    /// Ambient sends ?ID= and ?PASSWORD= — identity and secret in one request.
    FromQuery { id: &'static str, secret: Option<&'static str> },
    /// WeatherLink Live broadcasts its DSN in the packet; APRS-IS carries a callsign.
    FromPayload(fn(&Frame) -> Option<StationKey>),
    /// A peer registers and is issued an id it presents thereafter.
    FromRegistration,
}
```

The prompt's observation is that connection direction and station cardinality are linked: a transport
we initiate is inherently 1:1, one they initiate is inherently 1:N. That is true of all seven existing
drivers, and the obvious move is to make it an invariant — `cardinality == One` iff the transport is
outbound.

**The flexibility test in §4a.7 breaks that invariant, which is what the test is for.** An MQTT
subscription is outbound-initiated *and* 1:N: one connection to one broker, a topic wildcard,
`weather/+/obs`, and the station identity is in the topic. So direction is not the rule. The rule is
one level down:

> **`cardinality == One` if and only if `identity == Fixed`.** Connection direction only sets the
> *default* identity strategy — outbound defaults to `Fixed`, inbound cannot be `Fixed`.

Checked at plan time: `StationIdentity::Fixed` with an inbound transport is a config error (that is
the Ambient bug, made unrepresentable), and any non-`Fixed` identity requires the station set to come
from the registry rather than the device row.

### 4a.5 All seven drivers, mapped

| Driver | Transport | Session | Codec | Identity | Trigger | Cardinality |
|---|---|---|---|---|---|---|
| Davis | `Serial` \| `TcpOut` | wake + `LOOP n`, re-issue | `DavisLoop` — fixed little-endian struct + CRC16, decoded with `zerocopy` (D-32) | `Fixed` | `Service` | 1 |
| Campbell | `Serial` \| `TcpOut` | command/response | `CampbellAscii` | `Fixed` | `Service` | 1 |
| Snow gauge | `GrpcOut { streaming: true }` | stream establish | `SnowGaugeProto` | `Fixed` | `Service` | 1 |
| AirGradient | `HttpOut { /measures/current }` | none | `AirGradientJson` | `Fixed` | `Every { 3s }` — the measured default | 1 |
| Ambient | `HttpIn { /ingest/ambient }` | none | `AmbientQuery` | `FromQuery { id: "ID", secret: Some("PASSWORD") }` | `Service` | **N** |
| WeatherLink Live (broadcast) | `UdpIn { 22222 }` | none | `WllConditions` | `FromPayload(did)` | `Service` | **N** |
| WeatherLink Live (re-arm) | `HttpOut { /v1/real_time }` | none | — (no readings) | `Fixed` | `Every { 0.8 × duration }` | 1 |
| WeatherLink Live (poll) | `HttpOut { /v1/current_conditions }` | none | `WllConditions` | `Fixed` | `Every { ≥30s }` | 1 |
| gRPC receiver | `GrpcIn` | none | `WeatherV1Proto` | `FromRegistration` | `Service` | **N** |

No driver fails to fit, but two need comment.

**WeatherLink Live is two transports feeding one codec family, plus a control call.** The prompt
predicts the first half and the mapping confirms it: `WllConditions` decodes both the UDP broadcast
payload and the HTTP `current_conditions` body, because they are the same condition records with
the same sensor-mapping logic — the Go tree already has this, as a single 427-line
`data_transformer.go` serving both paths. What the prompt does not anticipate is the third piece:
broadcast mode is not purely inbound, because the Go code has to call the device's HTTP API to arm
the broadcast and re-arm it before it lapses (`api_client.go:44 StartRealTimeBroadcast`,
`station.go:151 refreshBroadcast`). That re-arm is an outbound control request on a timer, which
decomposes cleanly into its own `Every` component — it talks to the station, not to our socket, and
shares no state with the listener. So WLL is **two components in broadcast mode** (a `UdpIn` service
and an `HttpOut` re-arm) or **one in poll mode**, and the apparent anomaly is entirely the re-arm,
which is not an anomaly once it has somewhere to live.

**Ambient becomes N stations on one endpoint, and the second station is a config row.** One
`station:ambient` component, `Transport::HttpIn { mount: "/ingest/ambient" }`, provides `HttpRoutes`,
mounted on the shared listener:

| | Today | Under this model |
|---|---|---|
| Second Ambient station | a second `http.Server` on a second port | one row in `devices` |
| Listener count | N | 0 (shares `http:listener`) |
| Port configuration | one port per device, opened by the device | none |
| Station identity | the port the request arrived on | `?ID=`, resolved against `StationRegistry` |
| Authentication | none — any request on the port is trusted as that station | `?PASSWORD=` compared in constant time against the registry secret (D-33) |

**`FromRegistration` generalises the remote-station registry.** It is one identity strategy of five:
the one where the key is minted by us at registration and presented on each subsequent reading. The
registry stops being `RemoteStationRegistry` hanging off a single device config and becomes the
registration path of the general `StationRegistry` capability — the same capability APRS uses for
coordinates and callsign, and the same one Ambient's `?ID=` resolves against. One registry, five ways
to arrive in it.

### 4a.6 Identity resolution is the authentication point

Today there effectively is not one. Ambient trusts any request arriving on its port as being that
station. The gRPC receiver is worse — it pushes the reading into the distributor **before** consulting
the registry, and an unknown sender is only debug-logged:

```go
// internal/weatherstations/grpcreceiver/station.go:174-194
internalReading := s.convertToInternalReading(reading)
select {
case s.ReadingDistributor <- internalReading:        // ← stored first
...
if reading.StationId != "" && s.registry != nil {    // ← checked second
    if remoteStation == nil {
        s.logger.Debugf("Received reading from unregistered station ID: %s", reading.StationId)
    }
}
```

**Specification: identity resolution is one code path, it runs before a reading reaches the stream,
and it is the only place authentication happens.**

```rust
pub enum Reject { UnknownStation, BadSecret, Disabled, UnauthorizedPeer }

/// The single gate. Pure apart from the registry read. Counters per Reject variant.
fn resolve(meta: &FrameMeta, id: &StationIdentity, reg: &StationRegistryHandle)
    -> Result<StationKey, Reject>;
```

Per transport:

| Transport | Credential | Policy |
|---|---|---|
| `HttpIn` | the protocol's own secret (`?PASSWORD=` for Ambient), constant-time compared (`subtle`, D-33) against the registry | reject on mismatch |
| `GrpcIn` | **mTLS**: the peer certificate maps to a station or a registration grant | reject unauthorised peers at the TLS layer, before any frame is decoded |
| `UdpIn` | none available — WLL broadcasts unauthenticated on the LAN | identity must be `FromPayload`; the station set is an allow-list from the registry; an unknown DSN is dropped and counted, never registered |
| `Serial`, `TcpOut`, `HttpOut`, `GrpcOut { streaming }` | n/a — identity is `Fixed`, the wire is the credential | — |
| `MqttSub` | broker credentials for the connection; topic → station from the registry allow-list | unknown topic dropped and counted |

Three consequences worth stating plainly, because each is a current defect closed by construction:

- A reading **cannot** reach the stream before identity resolution, because resolution produces the
  `StationKey` the envelope requires. There is no envelope without a key.
- An unknown sender is a **counted rejection**, not a debug log. `rejected_total{reason}` per
  component is the signal that someone is pointing a station at you.
- The secret lives in `StationRegistry` and is a `SecretString` (D-33), so it is redacted in debug
  output rather than printed — unlike the current management token, which is logged in cleartext at
  startup and on every failed authentication.

### 4a.7 Four new collection types, no new infrastructure

Each of these is a `(Transport, Session, Codec, StationIdentity, Trigger)` tuple and a codec. Nothing
else in the design changes.

| New station type | Transport | Session | Codec | Identity | Trigger | New infrastructure |
|---|---|---|---|---|---|---|
| **MQTT subscribe** | `MqttSub { broker, "weather/+/obs" }` | none | `MqttJson` | `FromPath` (topic segment) | `Service` | **one `Transport` variant** — and it is what corrected the cardinality invariant in §4a.4 |
| **Vendor cloud poll** (Tempest / WeatherFlow) | `HttpOut { vendor API, auth }` | none | `TempestJson` | `Fixed`, or `FromPayload(device_id)` for a multi-station account | `Every { 60s }` | none |
| **Modbus sensor** | `TcpOut` or `Serial` | **`Session`** — write the register read, read the response | `ModbusRtu` (pure, over the response frame) | `Fixed` | `Every { 30s }` | none — `Session` already exists for Davis |
| **APRS-IS as an input** | `TcpOut { noam.aprs2.net:14580 }` | login with callsign + passcode, then a server-side filter | `AprsWeatherReport` (pure) | `FromPayload(callsign)` | `Service` | none |

The APRS-IS input is the one the prompt singles out as inexpressible today, and it is worth spelling
out why it falls out for free here. It is one outbound TCP connection carrying weather reports from
**many** stations, identified by the callsign inside each packet. That needs: an outbound transport
(exists), a login session (exists, for Davis), a pure codec over a text packet (exists), payload-based
identity (exists, for WeatherLink Live), and N-stations-on-one-endpoint (exists, for Ambient and the
gRPC receiver). The current architecture cannot express it because "weather station" means "one
device, one config row, one connection" there — and `cardinality == One iff identity == Fixed` is the
rule that removes that assumption. The pleasing part is the symmetry: the same `pkg/aprs` gains an
encoder (§4b) and a decoder, and `FromPayload(callsign)` is the same strategy on both sides.

Modbus is the honest check on `Session`: a register read is request/response, and if `Session` had
been invented for Modbus it would be suspect. It was required by Davis first.

---

## 4b. Rate-limited outputs: the window is the hard part

### 4b.1 Rate limiting is structural; the value is the problem

A component with `Trigger::Every(interval)` does not wake more often than allowed, so rate limiting
needs no queue, no buffer and no coalescing — which is the §1 Decision that makes the D-30 amendment
necessary. What is left is the hard part: **what value to send**, because a point sample is usually
the wrong number and the protocols say so. CWOP wants wind direction and speed *averaged* with gust as
a *peak over ~5–10 minutes*. Weather Underground encodes the windows in its field names —
`windspdmph_avg2m`, `windgustmph_10m`.

The current code half-knows this and is inconsistent. One APRS packet draws from three sources:

| Value | Source today | Correct per CWOP |
|---|---|---|
| wind gust | `SELECT calculate_wind_gust(?)` — a dedicated SQL function over ~10 minutes | right idea, wrong place |
| wind direction | straight off the reading | **wrong** — an average is required |
| wind speed | straight off the reading | **wrong** — an average is required |
| day rain | `CalculateDailyRainfall` called inside the packet builder | right value, wrong layer |

And the "latest reading" all of them start from is itself:

```go
// internal/database/client.go:121
c.DB.Table("weather_1m").
   Where("stationname=? AND bucket > NOW() - INTERVAL '2 minutes'", pullFromDevice).
   Limit(1).Find(&br)
```

`LIMIT 1` with **no `ORDER BY`**. It is an arbitrary bucket from a two-minute window, so it may be two
minutes stale, and because `weather_1m` is a 1-minute aggregate it is *accidentally* a 1-minute
average of some unspecified minute. PWS Weather and Weather Underground use the same function.

### 4b.2 An encoder declares the windowed values it needs

```rust
pub struct Window(Duration);                 // Window::INSTANT, Window::of(5min), Window::DAY

pub enum Agg { Last, Mean, CircularMean, Max, Min, Sum }

pub trait OutputEncoder {
    /// Declared, reviewable, line by line against the protocol specification.
    fn required(&self) -> &[(Metric, Window, Agg)];
    fn encode(&self, v: &WindowedView) -> Result<Vec<u8>, EncodeError>;
}
```

APRS/CWOP then declares a table instead of smearing the decision across a string builder:

| Metric | Window | Agg | CWOP requirement |
|---|---|---|---|
| `WindDir` | 5 min | `CircularMean` | averaged direction |
| `WindSpeed` | 5 min | `Mean` | averaged speed |
| `WindGust` | 10 min | `Max` | peak over the window |
| `OutTemp` | instant | `Last` | current |
| `Barometer` | instant | `Last` | current |
| `OutHumidity` | instant | `Last` | current |
| `RainLastHour` | 1 h | `Sum` | — |
| `RainDay` | local day | `Sum` | midnight-to-now, local |

That table is reviewable against the CWOP specification by someone who does not read Rust, which is
the actual deliverable. Weather Underground declares its own with `avg2m` and `10m` windows that match
its field names exactly, so the names stop being a thing you have to know and become a thing the code
says.

**`CircularMean` is not a new mechanism.** The Go tree already has the pattern — accumulate the sine
sum, the cosine sum and a count, finalise with an arctangent — as a `circular_avg` custom aggregate,
and W0 verified it composes correctly through all three aggregate levels to within 3 × 10⁻¹² degrees
(D-13). That is the pattern `Agg::CircularMean` names.

### 4b.3 Where the windows come from

`WindowedView` (§2.1) resolves a declaration by rolling up 1-minute aggregate buckets using each
metric's declared `agg` from the registry (W2). **This is the second payoff of storing composable
aggregate state** (D-13): because the 1-minute level holds `sum` and `count` rather than a finished
average, rolling ten buckets into a correct ten-minute mean is arithmetic — and D-12 measured what
happens otherwise, `avg(avg(x))` disagreeing with the direct average in 658 of 868 cases. The same
`agg` field in the registry that generates the continuous-aggregate SQL tells the encoder what to send.

Two consequences:

- **`window:rollup` is one component and the only thing that knows the roll-up arithmetic.** Wind gust
  stops being "a SQL function called from the radio component with a comment saying *same as
  website*". It is one declaration, one implementation, one owner — W3 §2a's rule for derived values.
- **No SQL in an output component.** `aprs:*`, `pws:*` and `wu:*` require `WindowedView` and **not**
  `ReadingStore`, so they cannot run a query; §2.3's resolve-once rule means they are never handed a
  handle that could.

`Window::DAY` is the one window that needs a time zone, and it takes it from the station (§1.6) —
which is the rollover the current `date_trunc('day', NOW())` gets wrong.

### 4b.4 The APRS structural cleanup

APRS is 991 lines across `controller.go` (859) and `kiss.go` (132), while `pkg/aprs` — the library
that should hold the protocol — is **two functions, 68 lines**, only passcode calculation.

**Transport dispatch becomes an exhaustive enum, because today it is a live bug.**

```go
// internal/controllers/aprs/controller.go:378
if strings.ToLower(device.APRSTransport) == transportKISS {
    a.sendReadingViaKISS(ctx, device, reading, windGustResult.WindGust)
    return
}
a.sendReadingToAPRSIS(ctx, device, reading, windGustResult.WindGust)
```

Two branches for three transports, and the third (KISS-serial versus KISS-TCP) is decided much
deeper, inside `openKISSConnection`, on a *second* stringly-typed field whose `default:` arm is TCP.
The `default:` on the outer branch is worse: an **unrecognised transport string silently transmits
over APRS-IS**, so `aprs_transport = "kis"` puts the weather report on the internet instead of the
radio, with no error anywhere.

```rust
pub enum FrameSink {
    AprsIs     { server: String, callsign: Callsign, passcode: Passcode },
    KissSerial { dev: PathBuf, baud: u32, path: Vec<Callsign>, dest: Callsign },
    KissTcp    { addr: SocketAddr,        path: Vec<Callsign>, dest: Callsign },
}
```

Exhaustively matched, parsed from configuration with `serde`, so **an unknown transport is a config
parse error** — at plan time, before anything transmits. And the two KISS variants collapse into one
implementation over an `AsyncRead + AsyncWrite`: the Go code half-discovered this already, since
`openKISSConnection` returns an `io.ReadWriteCloser` and both arms then run identical framing.

**Transport is a sink concern, not a component concern.** The prompt asks for the call. APRS is one
component per device whose job is (schedule, declare windows, encode); where the frame goes is a
`FrameSink` selected by configuration. The reason is that the encoder's output is *shared*: there are
**two framings over one information field** — TNC2 text for APRS-IS, AX.25 for both KISS variants —
so the split falls at `encode_info() -> InformationField` (pure, shared by all three) and
`FrameSink::send(&InformationField)` (framing + transport, three implementations, two framings).
Making transport a component concern would duplicate the encoder across three components; making it a
sink concern duplicates nothing.

**Twelve pure functions move into `pkg/aprs`.** `CreateCompleteWeatherReport`,
`buildWeatherReportInfo`, latitude/longitude conversion, `AltitudeCompress`, `CourseCompress`,
`SpeedCompress`, `LatPrecompress`, `LonPrecompress`, `EncodeBase91Position`,
`EncodeBase91Telemetry`, and a hand-rolled `round()` are all pure and all currently sit in the
controller, entangled with database access and transport. In the new crate they are golden-testable
against captured packets — impossible today for exactly that entanglement reason, which is why
`aprs/controller_test.go` is 114 lines.

**One component instance per device**, deleting `reconcileWorkers`, its
`map[string]context.CancelFunc`, its private `sync.WaitGroup`, the 60-second reconcile ticker and the
five-level call chain (§4.2).

**The bespoke lifecycle and health monitor are deleted** (§3.2). `Start`, `Stop`, `IsRunning`,
`GetHealth` and the 90-second `startHealthMonitor` ticker that writes into a package-level
`storage.GlobalHealthManager` all go; component state is the health surface, and the one useful part
becomes a `/test/*` check.

**No SQL in the radio component** (§4b.3).

### 4b.5 Floors, retry ordering, and pushback

**Minimum intervals are enforced at plan time, not per send.** The floors exist —
`MinPWSUploadInterval` and `MinWUUploadInterval`, both 60 s — but `ResolveUploadInterval` clamps and
logs a warning on **every call**, which for a 60-second uploader is a warning per minute forever, and
which means the configuration in the database still says the wrong thing. Under this model the
interval is validated once, in `plan()`, and for HTTP services it is clamped there with a single
warning naming the component.

**Retry goes outside RateLimit, and each attempt takes its own permit.**

```rust
// tower ServiceBuilder applies layers outside-in, in declaration order.
ServiceBuilder::new()
    .layer(RetryLayer::new(policy))      // outermost — a retry re-enters the limiter
    .layer(RateLimitLayer::new(1, period))
    .layer(TimeoutLayer::new(attempt_timeout))
    .service(http)
```

Reversed — `RateLimit` outside `Retry` — every retry happens *inside* a single permit, so the first
failure spends one permit and then hammers the service as fast as the retry policy allows. That is
precisely the burst the rate limit exists to prevent, and it is worst exactly when the remote service
is already struggling. `Timeout` is innermost so it bounds an *attempt* rather than the whole retry
sequence, which is what makes a retry reachable at all.

This ties back to §1.4: the tick deadline is the outer bound on the whole stack, so
`deadline >= retries × (attempt_timeout + rate_limit_period)` must hold, and it is checked in
`plan()` alongside `deadline <= period`. A retry budget that cannot fit inside the deadline is a
configuration error, not a mysterious stream of `deadline_exceeded`.

**Adaptive pushback.** Weather Underground and PWS Weather can answer `429` with `Retry-After`. The
limiter accepts an external "not before T" signal, and the supervisor skips any occurrence falling
before T, counting it as a skip with a distinct reason. So a service telling us to slow down actually
slows us down, instead of being retried on the next fixed tick.

**The radio asymmetry, and the floor as a type.** An HTTP service can tell you to slow down. A radio
cannot: APRS over KISS has no `429`, and flooding a shared band has no feedback mechanism at all.
Clamp-and-log is proportionate for APRS-IS and HTTP; for the KISS transports the local floor is the
only protection there is, so it is a constraint in the type rather than a configuration value with a
warning:

```rust
/// Only constructor. Rejects anything below the RF floor. There is no `From<Duration>`.
pub struct RadioInterval(Duration);
impl RadioInterval {
    pub const FLOOR: Duration = Duration::from_secs(600);
    pub fn new(d: Duration) -> Result<Self, TooFast> { /* d >= FLOOR or Err */ }
}

// KissSerial and KissTcp take a RadioInterval. AprsIs takes a Duration, clamped at plan time.
```

A `FrameSink::KissSerial` or `KissTcp` cannot be constructed with a faster interval, so a too-fast RF
beacon is a config parse error and not a runtime warning. The floor is **600 s**, which is the one
number in this document that changes existing behaviour — today's default
`aprs_upload_interval` is 300 s and applies to RF and internet alike. It is called out in **Open
questions** for that reason.

---

## 5. Interaction with the reading pipeline

`OnReading` is one trigger among six. W3 owns the pipeline internals; this document owns the boundary,
which is three things: what the stream carries, how a component subscribes, and what happens when a
component cannot keep up.

**What it carries.** Envelopes, not readings: `(StationKey, observed_at, received_at, provenance,
Reading)`. The key exists because identity resolution produced it (§4a.6) — there is no envelope
without one, which is the structural reason an unauthenticated reading cannot be stored. Provenance is
W7's (multi-hop loop prevention); the stream carries it opaquely.

**How a component subscribes.** It does not. It declares `Trigger::OnReading { filter, policy,
deadline }` and the supervisor delivers batches to `OnReading::readings`. No component holds a channel
receiver, which is what makes the delivery policy a supervisor concern rather than N local decisions.

```rust
pub enum ReadingFilter {
    All,
    Stations(StationSet),
    /// Loop prevention for multi-hop topologies. W7 §2 owns the semantics.
    NotFrom(OriginId),
}

pub enum ReadingPolicy {
    /// Must not lose data. Storage writer, collector uplink.
    Durable,
    /// Only the newest matters. Snapshot publisher, SSE/live stream.
    Latest,
    /// Lossy is acceptable; skips are counted. Metrics, sampling.
    Lossy,
}
```

`Stations(..)` is not speculative: APRS currently filters at send time by calling
`station.Capabilities().Has(weatherstations.Weather)` inside `sendStationReading`, and the gRPC
receiver's `Capabilities()` carries a `TODO` saying it cannot report what its remote stations actually
measure. Filtering belongs at the subscription, declared, not inside a send path.

**The invariant, and the one thing it forces.**

> **A source is never blocked by a sink.** No reading policy may back-pressure a station driver.

This is non-negotiable because the current violation is severe. The reading distributor performs a
blocking, unguarded send per engine into a 20-deep channel:

```go
// internal/managers/storage.go:211
for _, e := range s.Engines {
    e.C <- r        // blocking, unguarded, one per engine
}
```

so a stalled TimescaleDB back-pressures the serial read loops — a Davis console that is not drained
loses packets — and it can hang shutdown (D-29).

The invariant has a consequence that must be stated rather than assumed, because it is the one place
this document's boundary pushes work onto W3 and W7: **`Durable` cannot be satisfied by an in-memory
channel alone.** If the stream never blocks and the queue is bounded, a full queue means loss; if the
queue is unbounded, a stalled database is an OOM. So durability requires a **local durable spool at
the fan-out point** — the writer and the uplink drain from the spool rather than from the live
channel, and a slow sink falls behind on disk instead of upstream. W7 §3 already designs an acked,
resumable spool for the collector; the same mechanism serves `writer:timescale`. The degenerate case
is explicit: if the spool itself cannot be written, that is fatal for the process, not a silent drop.

`Latest` is a `watch` — coalescing by construction, never blocking, and the newest value is the only
one that matters for a snapshot or an SSE push. `Lossy` is a `broadcast` whose overflow increments
`lagged_total` for that component. A `Durable` component that exceeds its deadline repeatedly is
`deadline_exceeded` plus a growing spool depth, both of which are visible — which is the difference
between a slow sink being a diagnosable condition and being an unexplained ingest stall.

---

## 6. What this deletes

A tally, because the model should be judged on what it removes as well as what it adds.

| Deleted | Why it existed |
|---|---|
| 21 hand-rolled scheduling clocks, plus an unused generic periodic-task helper | no supervisor owned scheduling (D-25) |
| the 15-second "initial upload" timer, ×2 services | no `FirstRun` policy |
| the snow cache's 30-second wait-for-data loop | no scheduler already ticking |
| two 3-second live-update poll loops | no `OnReading` subscription |
| APRS `reconcileWorkers` + `map[string]context.CancelFunc` + private `WaitGroup` + 5-level call chain | one controller managing N devices |
| APRS `Start`/`Stop`/`IsRunning`/`GetHealth` + the 90-second health ticker + `GlobalHealthManager` | `Controller` declared only `StartController()` |
| `interfaces.AppReloader` and the management special-case in the controller manager | a genuine dependency cycle (§2.5) |
| 4 constructor-time `ValidateTimescaleDBConfig` refusals | missing dependency meant refusing, not waiting (D-27) |
| N `http.Server`s for N Ambient stations, plus their per-device ports | station cardinality not modelled |
| `RemoteStationRegistry` as a device-config bolt-on | `FromRegistration` not recognised as one strategy of five |
| `SELECT calculate_wind_gust(?)` inside the radio component | no owner for derived values |
| the `sun_times` table, its 366-row rebuild, and the precompute's place in the REST constructor | a pure function stored in a database (§4.3) |
| 7 listener-owning sites | `HttpRoutes` not a capability |
| the stringly-typed APRS transport if/else and its silent APRS-IS fallback | not an exhaustive enum |

---

# Decisions

1. Component **declaration is pure data derived from configuration and exists before construction**; `ComponentSpec` carries id, trigger, requires, provides and a builder that has not run.
2. `plan(&Config) -> Result<Vec<ComponentSpec>, PlanError>` is a **pure function**, so every structural error — unknown capability, duplicate exclusive provider, unprovided requirement, cycle, bad deadline, sub-floor interval, impossible identity — is a unit test and a CI check, never a runtime surprise.
3. A **build failure is a lifecycle transition**, not a startup abort; this replaces four controllers that refuse to construct without TimescaleDB and stay dead until the process is restarted.
4. **One declarative trait plus three executable traits** — `Tick`, `OnReading`, `Serve` behind a `Body` enum — because the three differ in what the supervisor supplies and in the completion contract. A single `tick` would lie about `Service`.
5. `OnReading` delivers a **batch**, not a single reading; a batch of one is the degenerate case.
6. The **supervisor owns every clock** (D-25); no component constructs a ticker. Sixteen hand-rolled clocks in the Go tree become supervisor state.
7. A `Service` component owns time *inside* its loop (read deadlines, reconnect backoff); that is not a schedule and not a D-25 violation.
8. **Every scheduled component declares a `deadline`, and `deadline <= period` is enforced at plan time**, so overlap can only be momentary and `Skip` is the only overlap policy needed. `skipped_total` and `deadline_exceeded_total` are exported per component.
9. **Jitter is a deterministic phase offset derived from the component id**, not per-tick randomness — stampede is solved by phase separation, and randomness would destroy the predictability of "next run at".
10. `FirstRun::AtPhase` is the default and `Immediate` is opt-in; the hard-coded 15-second initial upload goes away.
11. `Calendar` time zones are **IANA names resolved per station** (`TzSource::Station`), validated at plan time; a fixed offset is rejected because it cannot express DST. Today the device `timezone` field is ignored by every rollover that claims to use it.
12. Every `Calendar` firing has an **occurrence key** `(component_id, local_date, slot)`, and the supervisor persists the latest occurrence's outcome.
13. **DST: a nonexistent local time fires at the first instant after the gap; an ambiguous local time fires on the first occurrence only**, deduplicated by occurrence key.
14. **Missed windows default to `Misfire::RunLatest { grace: 24h }`** — every `Calendar` user here is idempotent and only-the-latest-matters; `misfired_total` is surfaced.
15. The capability set is `ReadingStream`, `ComponentControl`, `ConfigStore`, `ConfigAdmin`, `StationRegistry`, `ReadingStore`, `ForecastStore`, `AlertStore`, `SnowCache`, `WindowedView`, `LatestSnapshot`, `HttpRoutes` — each justified by an existing component.
16. **`SunTimes` is not a capability and no `Reconfigure` capability exists**: sun times are a pure function (§4.3), and the config watch is a root fact in every `Cx` rather than an edge everything declares.
17. Requirements carry **strength (`Hard`/`Soft`) and access (`Read`/`Write`)**; only `Hard` edges form the ordering graph, which makes `Soft` the escape hatch for any legitimate cycle.
18. Capabilities have **arity**: `Exclusive` (one provider; duplicates are a plan error) or `Additive`.
19. **`HttpRoutes` is additive and late-bindable; the listener never waits for all route providers**, so the management UI is reachable precisely when something else is broken. An unmounted path returns `503` naming the component and what it waits on.
20. **Capability availability is a lifecycle concern; data presence is a response concern.** `/forecast` depending on Aeris having fetched is a *freshness* declaration consumed by W5 and `/test/*`, never by the scheduler.
21. Capabilities reach a component as **typed handles resolved once in `build`**, not a registry lookup at use time — so after `build` a component cannot observe absence, and "the database is gone" is a lifecycle state rather than an error branch in business logic.
22. **Readiness is explicit and distinct from `Running`**, published through a per-capability watch, which is what makes revocation and therefore a re-enterable `Waiting` possible.
23. **Cycles are detected in `plan()` and fatal there**, reported as a config error naming the path. The topological order is an optimisation, not a correctness requirement, because a component started early merely waits.
24. The **`AppReloader` cycle is eliminated** by splitting the back-reference into `ConfigAdmin` (provided by `config:sqlite`) and `ComponentControl` (provided by the supervisor, a root). Nothing calls "reload"; the supervisor is a config-watch subscriber.
25. **Reconfiguration is a plan diff, not a restart**: unchanged components are untouched, and because `plan()` is pure an invalid configuration is rejected before anything is stopped.
26. Lifecycle is `Planned → Waiting{on} → Starting → Running{degraded} → Failed → Backoff → …`, plus `Finished`, `Stopping`, `Stopped`. **`Waiting` carries the capability *and its provider*** so W5 can render an actionable sentence.
27. **Degradation is a field on `Running`**, not a separate state.
28. **The supervisor drives every transition exclusively**; a component reports an outcome by returning `Ok`, returning `Err`, or being cancelled. There is no state setter, no health method, no `IsRunning`. Component state is the only health surface.
29. **For `Every`/`Calendar` the period is the backoff**; exponential backoff with full jitter (1 s base, 60 s cap) applies only to `Starting` failures and `Service` exits. Two clocks for one retry is one clock too many.
30. **No permanent give-up.** Backoff caps and keeps trying; what makes that safe is that `restart_total`, `attempt` and `last_error` are visible state.
31. **A `Failed` component does not cascade.** Dependents keep their last good handle; they return to `Waiting` only when a provider *revokes* a capability, and only for genuine unusability. A database outage therefore does not touch `/latest`.
32. **Stopping is cancellation plus `Drop`** (D-24); shutdown walks the topological order in reverse with a grace deadline, then aborts and counts.
33. **One component, one trigger.** Aeris becomes four components (two forecast spans, alerts, expired-alert sweep) and the snow cache becomes three, because they have independent periods, failures and rate budgets — and "alerts failing, forecasts fine" must be expressible.
34. `db:postgres` (owns the pool, publishes store handles) is **split from** `writer:timescale` (consumes the stream), so what the uploaders wait on is "is the database reachable", not "is the writer keeping up".
35. **The `/latest` handler performs no queries** — not fewer, none. `api:legacy` requires `LatestSnapshot` and is not handed a `ReadingStore` handle, so it cannot.
36. **The sun-times precompute is deleted**, not rescheduled: the table is day-of-year keyed with no horizon to exhaust, so neither offered trigger applies, and a pure Meeus evaluation is cheaper than the lookup it replaces. Fallback shape recorded if W1's fixtures disagree.
37. The two 3-second live-update poll loops become **one `OnReading { Latest }` component**.
38. `/test/*` connectivity checks are **`OnDemand` components** — including the one useful part of the APRS health monitor. Every `Tick` component is additionally run-now-able through `ComponentControl`.
39. "Driver" decomposes into **four** parts, not three: `Transport → Session → Codec → Identity`. The `Session` is the impure protocol conversation (Davis wake + `LOOP n`, Campbell command/response, WLL broadcast re-arm, gRPC stream establish) and it exists **so that `Codec` stays pure** and golden-testable against captured frames.
40. **`HttpPoll` is not a transport.** `Transport::HttpOut` is one request per tick and `Trigger::Every` owns the cadence, because a transport carrying `every: Duration` would be the one place in the design where a component schedules itself.
41. Each `Transport` variant **implies its trigger**, checked at plan time.
42. **`cardinality == One` if and only if `identity == Fixed`.** Connection direction only sets the default. The naive direction-based invariant was broken by the MQTT flexibility test, which is what the test is for.
43. **Ambient becomes N stations on one endpoint**: one `HttpIn { /ingest/ambient }` component, identity from `?ID=` against `StationRegistry`, secret from `?PASSWORD=` compared constant-time. A second Ambient station is a config row, not a second HTTP server.
44. **WeatherLink Live is two transports feeding one codec family plus an outbound re-arm**; the re-arm is its own `Every` component, which is the whole of the apparent anomaly.
45. **`FromRegistration` is one identity strategy of five**, so `RemoteStationRegistry` generalises into `StationRegistry`'s registration path instead of being special-cased onto a device row.
46. **Identity resolution is the single authentication point and runs before a reading reaches the stream** — there is no envelope without a `StationKey`. mTLS for `GrpcIn`, the protocol's own secret for `HttpIn`, a registry allow-list for the inherently unauthenticated `UdpIn`. An unknown sender is a counted rejection, not a debug log.
47. **Rate-limited outputs are not stream subscribers**; they are scheduled producers reading a `WindowedView`. This is the proposed D-30 amendment.
48. **An `OutputEncoder` declares `&[(Metric, Window, Agg)]`** and receives a `WindowedView`; APRS's CWOP table is reviewable line by line against the specification.
49. **Window summaries roll up 1-minute aggregate buckets using the registry's declared `agg`** (D-13), which is why the 1-minute level stores `sum`/`count` and not a finished average (D-12).
50. **No SQL in an output component**: `aprs:*`, `pws:*`, `wu:*` require `WindowedView` and are never handed `ReadingStore`.
51. **APRS transport becomes an exhaustive `FrameSink` enum** over `AprsIs`/`KissSerial`/`KissTcp`; an unknown transport is a config parse error, closing a live bug where a typo silently transmits over the internet instead of the radio.
52. **Transport is a sink concern, not a component concern**, because the information field is shared across two framings and three transports; the split is at `encode_info()`.
53. The **two KISS variants collapse into one implementation** over `AsyncRead + AsyncWrite`.
54. **Twelve pure APRS encoding functions move into `pkg/aprs`**, where they become golden-testable against captured packets.
55. **One APRS component per device**, deleting `reconcileWorkers`, its cancel map, its private `WaitGroup`, its reconcile ticker and a five-level call chain.
56. **Minimum intervals are enforced once in `plan()`**, not clamped-and-logged on every call.
57. **`Retry` goes outside `RateLimit`, `Timeout` innermost**, so each attempt takes its own permit; reversed, the first failure spends one permit and then bursts through the limit.
58. `deadline >= retries × (attempt_timeout + rate_limit_period)` is **checked in `plan()`**, so a retry budget that cannot fit is a config error rather than a stream of deadline misses.
59. **`Retry-After` is honoured**: the limiter accepts a "not before T" signal and the supervisor skips occurrences before T, counted with a distinct reason.
60. **The RF floor is a type, not a config value.** `RadioInterval`'s only constructor rejects anything below 600 s, so a too-fast KISS beacon is a parse error. APRS-IS and HTTP keep clamp-and-log, because they have a feedback channel and a radio does not.
61. **A source is never blocked by a sink.** No reading policy may back-pressure a station driver.
62. **Therefore `Durable` requires a local durable spool at the fan-out point**, not an in-memory channel; W7 §3's spool serves `writer:timescale` too. If the spool cannot be written, that is fatal, not a silent drop.
63. `ReadingPolicy` is `Durable` | `Latest` (a coalescing watch) | `Lossy` (counted), and `ReadingFilter` is `All` | `Stations(set)` | `NotFrom(origin)`.
64. The stream carries **envelopes** — `(StationKey, observed_at, received_at, provenance, Reading)` — and components never hold a channel receiver; the supervisor delivers.

---

# Open questions

Stated as decisions for the owner, with a recommendation each.

**OQ-A. Is the RF beacon floor 600 s or 300 s?**

Decision 60 makes the KISS floor a type-level constraint at **600 s**. This is the one number in this
document that changes existing behaviour: today's default `aprs_upload_interval` is 300 s and applies
to RF and APRS-IS alike, so an existing KISS user configured at 300 s would get a config parse error
on upgrade rather than a warning.

*Recommendation:* **keep 600 s for KISS.** Ten minutes is ordinary practice for an RF weather beacon
on a shared band, it matches the 10-minute gust window the CWOP encoding already uses, and the
asymmetry argument only works if the floor is actually conservative. Mitigate the upgrade edge in W9's
migration: a KISS device configured below 600 s is raised to 600 s by the migration with a one-line
report, rather than failing the parse on first boot. If the owner wants 300 s, the floor is one
constant and the type-level mechanism is unchanged.

**OQ-B. Does deleting the `sun_times` table risk the frozen contract?**

Decision 36 replaces a stored, day-of-year-keyed table with a pure Meeus evaluation. The values should
match, and the stored table is year-agnostic and therefore slightly wrong in non-leap years, but
"should" is doing work here and W1's fixtures are the only way to know.

*Recommendation:* **delete it, and make the check explicit in W1.** Add to W1's fixture set a
`/latest` capture for each configured station, and have the conformance runner compare sunrise/sunset
to the minute against a fresh evaluation. If they diverge, reinstate the component in the shape given
in §4.3 — one row in the plan. Carrying a table, a capability, a component and a startup dependency
forward to avoid a fixture comparison is the worse trade.

**OQ-C. Who owns `aeris:alerts:gc`?**

It is a retention sweep over one table with no API call, listed in §4.3 as a `Calendar` component
because that is where it lives today.

*Recommendation:* **W4 absorbs it as a retention policy** if the alerts table becomes a hypertable or
otherwise gains one; otherwise it stays as the `Calendar` component specified here. Either is correct;
what matters is that it is not lost in the move, which is why it has a row.

**OQ-D. Does the supervisor's `Calendar` occurrence state live in SQLite?**

Decision 12 requires the supervisor to persist the latest occurrence outcome per `Calendar` component
so that `Misfire::RunLatest` is correct across a restart. That is a small amount of durable state the
supervisor owns, and the supervisor is a root that currently requires nothing.

*Recommendation:* **the config SQLite database, in a `component_schedule` table**, written through a
narrow interface the supervisor holds directly rather than through `ConfigStore`. It is the only
durable store guaranteed present, it is already the supervisor's peer in the boot sequence, and the
state is tiny (one row per `Calendar` component). The alternative — keeping it in memory and treating
every restart as a misfire — is tolerable given `RunLatest` is idempotent, but it means a restart loop
re-runs every rollover. **W3 decides where the supervisor's own state lives**; this is the
recommendation.

**OQ-E. `Access::Read` / `Access::Write` on requirements — worth the ceremony?**

Decision 17 puts an access mode on every `Need`, so `api:legacy` requiring `ForecastStore` for reading
cannot write to it and `web:site:*` cannot write sun times or alerts.

*Recommendation:* **keep it.** It is two lines in the spec and one in the handle resolution, it makes
`requires` a readable least-privilege declaration, and it makes capability mocking in tests
granular. The alternative — one handle type per capability with whatever methods it needs — collapses
to the same thing with less information in the declaration. Low confidence that it earns its keep for
the *small* capabilities; high confidence for `ConfigAdmin` and the stores.

---

# Assumptions about other work items

Every assumption this document makes about a document it did not write. This is the drift detector.

**About W3 (service architecture)**

1. W3 owns `ReadingStream`'s internals — fan-in, fan-out, buffer sizes — and implements the §5
   invariant that **a source is never blocked by a sink** (Decision 61).
2. W3 implements `Durable` with a **local durable spool at the fan-out point**, not an in-memory
   channel (Decision 62), sharing W7 §3's mechanism. If W3 satisfies `Durable` with a bounded channel,
   either Decision 61 or Decision 62 is violated.
3. W3 accepts that a `Service` component owns time inside its loop and that this is not a D-25
   violation (Decision 7).
4. W3 owns `LatestSnapshot`'s contents, serialisation and derived-value rules (§2a), and honours
   Decision 35: the `/latest` handler performs **no** queries and is not handed a `ReadingStore`
   handle.
5. W3 mounts every route provider on **one listener** with axum + tonic (D-31), including the
   management API, the Ambient ingest path and both gRPC services, and implements the late-binding
   rule in Decision 19.
6. W3 owns where the supervisor's own durable state lives (OQ-D).
7. W3's crate layout places the pure APRS encoder in a crate with no database or transport dependency
   (Decision 54); a golden test must be able to link it alone.
8. W3 treats the Go emulators as fixed external test infrastructure (D-40), which this document relies
   on for `Session` and `Codec` testing without hardware.
9. W3 defines `ComponentError` as a typed error preserving its cause chain (D-34), since `Failed`
   carries it and W5 renders it.

**About W2 (metric registry)**

10. The registry carries a per-metric **`agg`** (D-13) that is the *same* field used both to generate
    continuous-aggregate SQL and to resolve an `OutputEncoder`'s `(Metric, Window, Agg)` declaration
    (Decision 49). If the encoder needs a second, separate declaration of how a metric aggregates,
    §4b's payoff is lost.
11. `Metric` is a registry-generated type that an encoder can name in a const table, and the set of
    `Agg` variants this document uses — `Last`, `Mean`, `CircularMean`, `Max`, `Min`, `Sum` — is a
    subset of what the registry expresses.
12. The 1-minute aggregate level stores composable state (`sum`/`count`, or a two-step state), so a
    ten-minute mean is arithmetic over ten buckets (D-12, D-13). `WindowedView` is unimplementable
    over stored finished averages.
13. Which metrics exist at 1-minute resolution (OQ-5 in `DESIGN.md`) must include every metric any
    `OutputEncoder` declares with a sub-hour window — at minimum wind direction, wind speed, wind
    gust, temperature, humidity, barometer and incremental rain.

**About W4 (database schema)**

14. The `WindowedView` ring is refreshed from the 1-minute continuous aggregate on minute boundaries;
    W4's refresh policy makes a bucket readable within the `window:rollup` component's 10-second
    deadline of the boundary. If the aggregate lags by more than that, `window:rollup`'s period must
    grow and this document's `Phase::Aligned` choice needs revisiting.
15. W4 may absorb `aeris:alerts:gc` as a retention policy (OQ-C).
16. Daily/monthly/yearly rainfall boundaries are computed in the **station's** time zone, not the
    database server's (§1.6). Today's `date_trunc('day', NOW())` is wrong and W4 should not carry it
    forward.

**About W5 (management UI)**

17. W5 §0 owns the translation table and no term from this document reaches a screen (D-36).
18. W5 §2 renders `Waiting { on: Vec<(Capability, ComponentId)> }` as an actionable sentence naming
    the provider, and renders `Running { degraded }` as "running, with X unavailable" rather than as a
    distinct status.
19. W5 consumes `ComponentControl` for state, start/stop and **run-now**, and the `/test/*` checks are
    `OnDemand` components (Decision 38) rather than bespoke handlers.
20. W5 surfaces the per-component counters this document defines: `skipped_total`,
    `deadline_exceeded_total`, `misfired_total`, `failed_total`, `restart_total`, `rejected_total{reason}`,
    `lagged_total`, plus `last_run`, `next_run`, `last_error` and `Backoff { until }`.
21. W5 displays `Freshness` (Decision 20) — "forecast last updated 3 h ago / never" — as distinct from
    component state, because an available-but-empty store is not a failure.

**About W7 (collector)**

22. W7 owns provenance and the `ReadingFilter::NotFrom(origin)` semantics (§5).
23. W7's acked, resumable spool is the mechanism `writer:timescale` uses for `Durable`, not a
    collector-only facility (Decision 62).
24. W7's remote-station registration is `StationIdentity::FromRegistration` against the general
    `StationRegistry`, with mTLS as the peer credential (Decision 46), and not a second registry.
25. The collector uplink is a `Trigger::OnReading { NotFrom(self), Durable }` component with no
    bespoke lifecycle.

**About W1 (frozen contract)**

26. W1's fixtures pin what `/latest` returns when the snapshot is **empty** (no reading has arrived),
    because Decision 20 makes that a defined response rather than a reason to hold the listener down.
27. W1's fixtures pin sunrise/sunset to the minute so Decision 36 is verifiable (OQ-B), and the
    conformance runner compares them against a fresh evaluation rather than only against the Go
    binary.
28. W1 states whether a `503` on an unmounted route (Decision 19) can ever be observed on a frozen
    path. The intent is that it cannot — `api:legacy` requires only `LatestSnapshot` hard — but W1
    owns the surface.

**About W6 (reconciliation)**

29. W6 reconciles the D-30 amendment proposed here against whatever W3 assumed about sinks, since D-30
    is the one shared decision this document diverges from.
30. W6 records the `sun_times` deletion (Decision 36) against the README capability inventory, which
    currently assigns "Sun-times precompute" to W2a §4 and now has no owner by design.

---

# Proposed amendments to `DESIGN.md`

Per the protocol: the decision below is **not** edited in place, and this document carries on
consistently with the existing decision's intent rather than silently overriding it.

> **2026-10-28 — D-30 proposed amendment by W2a.**
>
> **D-30 currently reads:** "Sinks are composable and each carries its own delivery policy. Durable
> sinks must not lose data; rate-limited upload sinks coalesce to the newest reading."
>
> **What was found.** Measured against the code, the rate-limited outputs are not sinks on the reading
> stream and cannot be made into correct ones. APRS/CWOP, Weather Underground and PWS Weather are
> driven by `time.NewTicker` and poll the database; none subscribes to readings. More importantly they
> *must not* send the newest reading: CWOP requires wind direction and speed **averaged** over ~5
> minutes with gust as a **peak over ~10 minutes**, and Weather Underground's field names encode the
> windows explicitly (`windspdmph_avg2m`, `windgustmph_10m`). Coalescing to the newest reading yields
> an instantaneous sample where an average is required — which is exactly the defect the current APRS
> packet builder has, taking gust from a 10-minute SQL function but wind direction and speed straight
> off the reading. The "latest reading" those uploaders start from is itself
> `weather_1m ... WHERE bucket > NOW() - INTERVAL '2 minutes' LIMIT 1` with **no `ORDER BY`**
> (`internal/database/client.go:121`), so it is an arbitrary bucket up to two minutes stale.
>
> **What replaces it.** Two decisions, splitting the asymmetry D-30 correctly identified:
>
> - **D-30a.** Durable sinks must not lose data. Because a source is never blocked by a sink, a durable
>   sink is served by a **local durable spool** at the fan-out point rather than by an in-memory
>   channel; a slow sink falls behind on disk, never upstream.
> - **D-30b.** Rate-limited outputs are **not stream sinks.** They are scheduled producers with a
>   declared period, and each declares the **windowed values** it needs as `(Metric, Window, Agg)`,
>   resolved by rolling up aggregate buckets. Rate limiting is therefore structural — a component
>   that does not wake more often than allowed needs no queue, no buffer and no coalescing.
>
> **Why the original was wrong rather than merely inconvenient.** D-30's *intent* — never backfill a
> rate-limited service with an hour of history — is correct and is preserved. Its mechanism encodes
> the assumption that an output's correct value is a *reading*, and for every rate-limited output in
> this system it is a *window*. An implementation built on D-30 as written would transmit
> instantaneous wind values to CWOP, which is a protocol violation that looks like working software:
> the packets are accepted, the numbers are wrong, and nothing reports it. That is the current
> behaviour, and D-30 as written would carry it forward.

No other `DESIGN.md` decision is contradicted. D-23 (retire "controller"), D-24 (cancellation, no stop
method), D-25 (declared trigger, no component-owned timer), D-26 (declared capabilities, supervisor
resolves order), D-27 (missing dependency means waiting), D-28 (configuration by watch, never shared
mutable state), D-31 (transport orthogonal to role) and D-36 (internal vocabulary is not user-facing)
are all implemented as written. D-27 is implemented more strictly than stated: this document moves the
*declaration* out of the constructor as well (Decision 1), because a declaration you can only read
after construction succeeds cannot drive a `Waiting` state.
