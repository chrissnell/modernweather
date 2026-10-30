# ModernWeather — management UI design

**Work item:** W5. **Produced by:** this document. **Depends on:** W3 (`docs/service-architecture.md`),
W2a (`docs/component-model.md`), W2 (`docs/metric-registry.md`), W1 (`docs/legacy-api-contract.md`).
**Consumed by:** W6, and the implementation work item that builds the UI.

**Scope.** The operator/admin interface only. The public weather website is frozen (D-38) and how it
is served is W3 §6–7; this document does not touch it. It writes no implementation code.

---

## Two things to read before §0

### 1. One proposed amendment to `DESIGN.md`, flagged and not applied

**A-W5-1 — D-35 says "one administrator".** It does not say whether a *second* browser session for
that one administrator is a second credential. This document reads D-35 as "one identity, many
revocable sessions" and designs accordingly, because the defect D-35 exists to fix is precisely that
today the session cookie *is* the API credential (`handlers.go:243-250`). If the intent was instead
"one session at a time", §3.3 changes materially. Carried on under the reading above, per the
amendment protocol. Full text in **Proposed amendments to `DESIGN.md`**.

Nothing else in this document contradicts a `DESIGN.md` decision. Three divergences from **W3** and
**W2a** — documents, not decisions — are recorded in **Assumptions about other work items** so W6 can
grep them: the management API's route prefix (which moves the log endpoint W3 §5.8 named), what
belongs in the management OpenAPI schema, and one extension to `ComponentControl`. They are
assumptions 20, 21 and 4.

### 2. The measured route count is 47, not 46

`docs/rewrite/README.md` and the W5 prompt both say the management API has **46 routes**. Counted
from `internal/controllers/management/controller.go`, it is **47** handler registrations reachable as
API endpoints (3 unauthenticated at lines 171–173, 44 under `/api` at lines 180–242), plus **9**
single-page-app page routes (lines 258–268) and **3** static-asset path prefixes (lines 248–251) —
**59 registrations in total**. §1 maps all 59, and additionally maps **two endpoints the shipped
JavaScript calls that do not exist on this router**, which the "46" framing cannot see because they
are counted on the client side, not the server side.

This is a counting correction, not a finding of substance. It is stated so that "all 46 routes are
mapped" cannot be satisfied while one is quietly missed.

---

## 0. Vocabulary — the user-facing language is a deliverable

### 0.1 The rule

> **No internal term reaches the screen.** `Component`, `Capability`, `Trigger`, `Source`, `Sink`,
> `Provision`, `Need`, `Envelope`, `Spool`, `ReadingStore`, `WindowedView`, `LatestSnapshot`,
> `HttpRoutes`, `controller`, `storage backend`, `engine`, `device` — none of these appears in a
> label, a heading, a button, a status line, an error message, a tooltip, a toast, a confirmation
> dialog, a validation message, or a URL path the operator can see in the address bar.

This is enforceable rather than aspirational, and §6.6 specifies the lint that enforces it: a banned
word list checked against every string literal in `.svelte` and user-facing `.ts` files, failing the
build. A term that must appear — in the "Advanced" raw-configuration view, say — is spelled with an
explicit escape (`raw('ReadingStore')`) so the exceptions are a greppable list rather than a hole in
the rule.

The hazard this exists against is specific and worth naming: W2a's vocabulary is *better for the
code*. "Component `aprs:backyard` is `Waiting` on `WindowedView`" is more precise than anything that
exists today, and is less comprehensible to a person who has just unboxed a weather station. Precision
in the code and plain language on the screen are not in tension — they are two different audiences —
but only if the translation is written down and checked.

### 0.2 The translation table

Refined from the prompt's starting point. Everything in the left column is internal; nothing in it is
ever shown.

| Internal / current | On screen | Notes |
|---|---|---|
| Controller | *never shown* | Split by what it does — the rows below |
| Component | *never shown* | Where one must be named, it is named by what it is: "your Davis station", "the Weather Underground upload" |
| Capability | *never shown* | §0.4 renders each one as the thing it is |
| Trigger / `Every` / `Calendar` / `OnReading` / `Service` / `OnStartup` / `OnDemand` | *never shown* | §0.4 renders each as a schedule sentence |
| Source / Sink | *never shown* | — |
| Envelope / reading pipeline / spool | *never shown* | A spool is **"unsent readings"**; its depth is **"N readings waiting to be sent"** |
| `restserver` controller | **Website & API** | One page: "My website" |
| `management` controller | **Admin** | It is the thing you are looking at; §1 drops its self-configuration page |
| `pwsweather` controller | **PWS Weather** | Under "Share my data" |
| `wunderground` controller | **Weather Underground** | Under "Share my data" |
| `aprs` controller | **APRS / CWOP** | Amateur-radio term kept deliberately — see §0.3 |
| `aerisweather` controller | **Forecasts & alerts** | Vendor named as a detail on the settings page, not as the feature |
| Storage backend | **Data storage** | "Where your readings are kept" |
| `timescaledb` storage | **Weather database** | Never "TimescaleDB" outside the connection fields and the diagnostics output |
| `grpcstream` storage | **Send my readings to another RemoteWeather** | — |
| `grpcreceiver` device | **Receive readings from another RemoteWeather** | — |
| Device | **Weather station** | Used consistently; today the UI alternates "device" and "station" within one screen |
| `snowgauge` device | **Snow depth sensor** | — |
| `ambient-customized` device | **Ambient Weather station (sends to us)** | — |
| `airgradient` device | **AirGradient air-quality sensor** | — |
| WeatherLink Live "sensor mapping" / template | **Which sensor is which** | §1.6 |
| Snow cache | **Snow calculations** | — |
| Aeris | **Forecasts & alerts** | — |
| Continuous aggregate / CAGG / rollup | **Summary history** | Appears only in the database diagnostics |
| `grpc_port` / gRPC | **App & integration port** | The iOS app connects here |
| Bearer token / auth token | *retired* — see §3 | Replaced by **Password** and **Sign-in sessions** |
| Reload configuration | *retired* — see §2 | Replaced by **Apply changes** / "Changes are live" |

**Two words that stay.** "Website" and "station" are the operator's own words — the owner runs a
weather website and owns weather stations — and replacing them would be jargon in the other
direction.

### 0.3 Three places plain language is the *wrong* call, stated so it is not "fixed" later

Plain language is the rule, not a reflex. Three terms are kept because the audience that sees them
already uses them, and translating would make the screen *less* clear:

1. **APRS / CWOP.** A person configuring this has a callsign and a passcode. "Share with amateur
   radio" as the *section* heading, "APRS / CWOP" as the service name, and the field labels keep
   `Callsign`, `Passcode`, `Symbol`, `Digipeater path` verbatim — those are the words on every other
   APRS tool and on the licence.
2. **Serial port / baud rate.** Someone plugging a Davis console into a Pi is holding a serial cable.
   "Connection speed" would be a worse label than "Baud rate", and the help line carries the
   explanation.
3. **Hostname, port, TLS certificate.** The person configuring a public website on their own domain
   already owns the domain. These keep their names; the help text explains what goes in them.

The test applied throughout: **would the word confuse the person who is about to type into this
field?** "Controller" fails it. "Baud rate" does not.

### 0.4 Status and error text — every internal state, rendered

This is where jargon does the most damage, because a confused operator is reading it at exactly the
moment they are already confused. Every lifecycle state from W2a §3.1 has a specified rendering.
`{subject}` is the component's user-facing name from §0.2 (for example "Weather Underground upload
for Backyard", "your Davis station"), `{provider}` is the plain name of the blocking provider.

| W2a state | Badge | Sentence |
|---|---|---|
| `Planned` | *not shown* | A planned-but-not-started component is invisible; the operator sees the first real state. Appears only in the Advanced view. |
| `Waiting { on: [(cap, provider)] }` | **Waiting** (info) | "Can't start yet — waiting for {provider as a thing}." One provider: "waiting for the weather database to connect." Two or more: "waiting for the weather database and snow calculations." |
| `Starting { attempt: 1 }` | **Starting** (info) | "Starting…" |
| `Starting { attempt: n>1 }` | **Starting** (warning) | "Starting — attempt {n}. The last try failed: {plain cause}." |
| `Running { degraded: [] }` | **Running** (success) | "Running." For scheduled things: "Running. Uploads every 10 minutes." |
| `Running { degraded: [caps] }` | **Running, limited** (warning) | "Running, but {missing thing} isn't available, so {consequence}." e.g. "Running, but forecasts aren't available, so the forecast panel on your website is empty." |
| `Failed { error, at, attempt }` | **Stopped** (danger) | "Stopped because of an error: {plain description}." Full cause chain behind a **Details** disclosure. |
| `Backoff { until, attempt }` | **Retrying** (warning) | "Retrying in {N} seconds." Counts down live. After 5 attempts: "Retrying in {N} seconds — {n} attempts so far." |
| `Finished { at }` | **Done** (neutral) | "Finished at {time}." Only `OnStartup` work and cleanly-removed items reach this. |
| `Stopping { since }` | **Stopping** (neutral) | "Stopping…" If over 10 s: "Stopping — waiting for it to finish." |
| `Stopped` | **Off** (neutral) | "Turned off." — because the only way an operator reaches `Stopped` deliberately is by turning something off. |

Triggers, rendered as schedule sentences:

| W2a trigger | On screen |
|---|---|
| `Every { period: 600s }` | "Uploads every 10 minutes." / "Checks every 10 minutes." — verb chosen per component |
| `Every { period: 60s, first: Immediate }` | "Updates every minute, starting as soon as it's running." |
| `Calendar { daily 03:00, Deployment }` | "Runs at 3:00 AM every day." |
| `Calendar { daily 00:10, Station(x) }` | "Runs at 12:10 AM every day, in the station's time zone (America/Denver)." |
| `OnReading` | "Runs every time a new reading arrives." |
| `OnStartup` | "Runs once when RemoteWeather starts." |
| `OnDemand` | *no sentence* — it is a button |
| `Service` | "Runs continuously." |

Counters, rendered (W2a Decision 20, W3 assumption 37):

| Counter | On screen | Shown when |
|---|---|---|
| `skipped_total` | "{n} runs skipped because the previous one was still going." | n > 0 |
| `deadline_exceeded_total` | "{n} runs gave up after {deadline}s." | n > 0 |
| `failed_total` / `last_error` | In the status line and the Details disclosure | always, if n > 0 |
| `restart_total` | "Restarted {n} times since RemoteWeather started." | n > 2 |
| `spool_depth_bytes` / `spool_lag_envelopes` | "{n} readings waiting to be sent ({size})." | n > 0 |
| `ring_dropped_total` | "{n} log entries dropped" — as the gap marker in the log view (§5.4) | on overflow |
| `admit_rejected_total{reason}` | "{n} readings rejected: {reason in plain words}" on the station page | n > 0 |
| `clock_step_total` | "The system clock jumped {n} times; schedules were re-based." | n > 0, Advanced only |
| `stop_timeout_total` | "{n} items had to be forced to stop." | n > 0, Advanced only |

Freshness (W2a Decision 20) and staleness (W3 §2a.6) are rendered **separately from state**, because
an available-but-stale thing is not a failure:

- `Freshness` → "Forecast last updated 3 hours ago." / "Forecast has never been fetched."
- `Staleness::Degraded { missing, since }` → "Some values are being held from 4 minutes ago: wind
  gust, rain totals."

### 0.5 Inline help — the standard

**Every label has a one-line explanation available in place.** Not a manual, not a separate docs
site, and not only a hover — hover does not exist on a phone, and an operator checking a station from
a phone in the garden is a real case.

The standard, applied to every field in every form:

```
Label                          ← 1–4 words, sentence case, plain
[ input                      ] ← the control
hint: one line, always visible, ≤ 90 characters, says what to put here and why
      ▸ More  ← optional disclosure: the 2–5 sentence version, examples, consequences
```

- The **hint is always visible**, not revealed on hover or focus. It is small and dim
  (`--color-text-muted`), and it costs one line.
- **"More" is a disclosure, not a tooltip.** Keyboard reachable, touch reachable, and its content is
  real prose rather than a sentence fragment.
- Hints are written for someone who has just bought a weather station. The test: *does this sentence
  make sense to someone who has never read a design document?* "The port the gRPC listener binds"
  fails. "The port the RemoteWeather phone app connects to. Leave it alone unless something else on
  this machine already uses 50051." passes.
- Help text lives in **one module** (`src/lib/copy/`), not inline in components, so the whole
  vocabulary can be reviewed and lint-checked as a unit (§6.6).

Worked examples, to fix the register:

| Field | Hint |
|---|---|
| Baud rate | "How fast the station talks over the cable. Davis consoles use 19200." |
| Base snow distance | "Distance in millimetres from your snow sensor down to bare ground, measured when there is no snow." |
| Wind direction correction | "Degrees to add if your wind vane isn't pointing true north. Leave at 0 if you aligned it." |
| Station time zone | "Used to decide when 'today' starts for daily rain and snow totals." |
| Upload interval | "How often to send a reading. Weather Underground asks for no more than one every 60 seconds." |
| Passcode (APRS) | "The numeric passcode for your callsign. If you don't have one, your station can still receive but not transmit." |
| SSL mode | "How to encrypt the connection to the database. Use 'require' unless the database is on this same machine." |
| App & integration port | "The port the RemoteWeather phone app connects to. Leave it alone unless something else on this machine already uses 50051." |

### 0.6 Navigation, organised by intent

A new user does not think "I need to configure a controller". They think "I want my data on Weather
Underground". The top level is therefore the set of things an operator wants, not the set of things
the daemon contains.

```
Home                     Everything at a glance
My weather stations      The hardware
Share my data            Weather Underground · PWS Weather · APRS/CWOP · Forecasts & alerts
My website               The public site and its settings
Data storage             The weather database, and sending/receiving between RemoteWeathers
System                   Activity · Diagnostics · This machine · Administrator · Advanced
```

Six items, not the prompt's five. **Home is the one addition**, and it is load-bearing rather than
decorative: §2 is about making "is this actually working?" answerable, and that question needs a
place to be answered on arrival. Home is also where the first-run checklist lives until it is
complete (§0.7), and where the root-cause rollup (§2.5) prevents five unexplained outages reading as
five problems.

**What is deliberately *not* a top-level item:**

- **"Admin" / the management API's own settings.** It is the thing you are looking at. Its port and
  certificate are a `System → Advanced` concern, not a peer of "My weather stations" (§1.4).
- **"Controllers".** Dissolved entirely — every former controller lands in the section that matches
  what it does for the operator.
- **"Storage".** Renamed and narrowed to "Data storage".
- **A dashboard of weather charts.** Out of scope; the public website already does that (D-38).

**Receiving from another RemoteWeather appears in two places, on purpose.** The *pairing* — peer
address, certificate, which peers are allowed — is configuration and lives under **Data storage**,
matching the §0.2 table. The stations that arrive over that link appear in **My weather stations** as
read-only entries badged "Received from {peer}", because that is where an operator looks for "what
stations do I have". One is configuration, the other is inventory; duplicating the *entry point* is
correct, duplicating the *editing* would not be.

### 0.7 First-run — the most important screen in the application

Today a new user meets a blank UI with tabs labelled "Controllers" and "Storage", which explains
nothing. This subsumes the "no stations configured" empty state from §8 and is the primary onboarding
flow, not an edge case.

**Shape: a resumable checklist, not a modal wizard.** Each step saves on completion and is
independently revisitable; closing the browser loses nothing; "Skip for now" is available on every
optional step and leaves the item on the Home checklist rather than hiding it. A modal wizard that
must be completed in one sitting is the wrong shape for a step whose success criterion is "readings
are arriving", which can take minutes.

```
┌─ Welcome to RemoteWeather ──────────────────────────────────────────┐
│                                                                      │
│  Let's get your weather station online. This takes about 5 minutes.  │
│                                                                      │
│  ✓ 1. Create your administrator account                    done      │
│  ▸ 2. Connect a weather station                       ← you are here │
│    3. Check that readings are arriving                               │
│    4. Choose where to keep your readings            optional         │
│    5. Put your weather on the web                   optional         │
│    6. Share your data with weather services         optional         │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘
```

**Step 1 — Create your administrator account.** The one-time setup claim (§3.4). This is the only
step that is *blocking*: nothing else is reachable until the instance is claimed.

**Step 2 — Connect a weather station.** The hardest screen in the product, so it is specified in
detail:

```
┌─ What kind of weather station do you have? ─────────────────────────┐
│                                                                      │
│  ○ Davis Vantage Pro / Vue        Connects by cable or network       │
│  ○ Davis WeatherLink Live         Connects over your network         │
│        ✓ We found one on your network at 192.168.1.42  [ Use this ]  │
│  ○ Campbell Scientific logger     Connects by cable or network       │
│  ○ Ambient Weather                Sends readings to us               │
│  ○ AirGradient air-quality sensor Connects over your network         │
│  ○ Snow depth sensor              Connects over your network         │
│  ○ Another RemoteWeather          Receives readings from a peer      │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘
```

- **Discovery runs automatically and before the choice**, not behind a "Discover" button on a form
  the user has already half-filled. WeatherLink Live announces itself over the network
  (`GET /api/v1/discovery/weatherlink`); a found unit is offered inline, with its address pre-filled.
- **Serial ports are enumerated into a picker**, never typed. A Davis chosen as "by cable" shows the
  actual ports present on the machine with their descriptions, from the serial-port enumeration that
  already exists (`handlers_system.go`). An empty list is an empty state with a cause: "No serial
  ports found. If your console is plugged in by USB, check the cable — or
  [choose network instead]."
- **Test before save, always.** The form's primary action is **Test connection**, and **Save** is
  secondary and enabled regardless. A failed test does not block saving — someone configuring a
  station that is not plugged in yet is a legitimate case — but it does show the failure and its
  likely cause before the operator walks away believing it worked. This inverts today's behaviour,
  where `/test/*` is a separate screen most users never find.
- **Only fields that matter for the chosen type are shown.** Today's form shows every field of
  `DeviceData` — 50+ columns spanning Davis serial settings, PWS credentials, APRS KISS TNC paths and
  WeatherLink sensor mappings — for every station type. Type selection comes first and the form is
  built from it.

**Step 3 — Check that readings are arriving.** A live panel, fed by the SSE stream (§5), not a poll
and not a "Test" button:

```
┌─ Backyard ───────────────────────────────── last reading: 2s ago ──┐
│   Temperature   54.3 °F      Wind      4 mph from the WSW          │
│   Humidity        38 %       Pressure  30.12 inHg                  │
│   ● Readings are arriving.                                          │
└─────────────────────────────────────────────────────────────────────┘
```

If nothing arrives within 60 seconds, the panel becomes the diagnostic rather than a spinner:
"No readings yet. {what we tried}. {most likely cause}. [Test connection] [Check the activity log]".

**Step 4 — Choose where to keep your readings.** Pre-offers the common answer ("Keep them on this
machine" / "Use a weather database I already have") and runs the database connectivity check inline.
Skippable: a station with no database still serves live readings, and saying so out loud is better
than implying the product is broken without one.

**Step 5 — Put your weather on the web.** Optional. Hostname, page title, which station feeds it.

**Step 6 — Share your data with weather services.** Optional. The §0.6 "Share my data" page, reached
with the first station pre-selected.

**When is first-run "done"?** When steps 1–3 are complete. The checklist then collapses to a single
dismissible line on Home. Optional steps that were skipped remain available from their own pages and
are not nagged about.

---

## 1. Functional inventory — all 59 registrations, mapped

Organised by operator task. Every route ends in exactly one of four dispositions:

- **Mapped** — a screen uses it (new API shape in §2/§4, same information).
- **Merged** — folded into another endpoint; the information survives, the route does not.
- **Dropped** — consciously removed, with the reason.
- **Not UI-facing** — it exists, but no screen calls it.

### 1.1 Authentication (3 registrations)

| # | Route | Disposition | Where it goes |
|---|---|---|---|
| 1 | `POST /login` | **Mapped**, rewritten | `POST /api/v1/auth/login` — now takes a **username and password** (today it takes a token only, `handlers.go:227-246`), returns a session, never echoes a credential. §3.3 |
| 2 | `POST /logout` | **Mapped** | `DELETE /api/v1/auth/session` — now actually revokes server-side, which today it cannot (the cookie *is* the credential). §3.3 |
| 3 | `GET /auth/status` | **Mapped** | `GET /api/v1/auth/session` — also reports whether the instance has been claimed, which drives the first-run redirect. §3.4 |

### 1.2 Weather stations (5 registrations)

| # | Route | Disposition | Where it goes |
|---|---|---|---|
| 4 | `GET /api/config/weather-stations` | **Mapped** | `GET /api/v1/stations` → **My weather stations** |
| 5 | `POST /api/config/weather-stations` | **Mapped** | `POST /api/v1/stations` → Add a station (§0.7 step 2) |
| 6 | `GET /api/config/weather-stations/{id}` | **Mapped** | `GET /api/v1/stations/{id}` → Station detail |
| 7 | `PUT /api/config/weather-stations/{id}` | **Mapped** | `PUT /api/v1/stations/{id}` → Station settings; response carries the config generation (§2.2) |
| 8 | `DELETE /api/config/weather-stations/{id}` | **Mapped** | `DELETE /api/v1/stations/{id}` → with a confirmation naming what else it breaks (§8.7) |

**The device-level enable/disable toggle** is the `enabled` field on `DeviceData`
(`pkg/config/provider.go:181`). It is **mapped** to a toggle on the station row and the station detail
header, labelled **"Collecting readings"**, and it is one of the places §2 matters most: today
toggling it writes the column and nothing stops, because `ReloadControllersConfig` never compares
contents (`internal/managers/controller.go:177-190`). Under the new model the toggle produces a plan
diff that stops the station component, and the UI shows that happening.

**One structural problem in the current schema, surfaced rather than inherited.** `DeviceData` carries
the *station's* hardware configuration **and** that station's PWS Weather, Weather Underground, APRS
and Aeris settings (`provider.go:196-240`) — while the global credentials for the same services live
in `ControllerData`. The same service is configured in two places. The UI resolves this by presenting
**one page per service** under "Share my data", which edits both halves together, and by *not*
showing upload settings on the station page at all beyond a read-only "Shared with: Weather
Underground, APRS" line that links there. Recorded as **OQ-W5-4**: the right fix is a schema change,
which is W4's ground, not W5's.

### 1.3 Data storage (4 + 2 registrations)

| # | Route | Disposition | Where it goes |
|---|---|---|---|
| 9 | `GET /api/config/storage` | **Mapped** | `GET /api/v1/storage` → **Data storage** |
| 10 | `POST /api/config/storage` | **Mapped** | `POST /api/v1/storage` |
| 11 | `PUT /api/config/storage/{id}` | **Mapped** | `PUT /api/v1/storage/{id}` |
| 12 | `DELETE /api/config/storage/{id}` | **Mapped** | `DELETE /api/v1/storage/{id}` |
| 13 | `GET /api/health/storage` | **Merged** | Into `GET /api/v1/runtime/components` (§2.3). Storage health stops being a parallel mechanism — W2a §3.2 deletes `storage.GlobalHealthManager` and makes component state the health surface. |
| 14 | `GET /api/health/storage/{type}` | **Merged** | Same; the per-item view reads one element of the component list. |

Note that the current `GET /api/config/storage` has **no** by-id read (`controller.go:196-199`) while
weather stations and websites do — an inconsistency the new surface removes.

### 1.4 Controllers → dissolved (5 registrations)

The `/api/config/controllers/{type}` family is the clearest case of an API shaped like the
implementation. It is keyed by **controller type**, so there can be exactly one `pwsweather`
configuration for the whole process, while per-station PWS settings live on the device row (§1.2).

| # | Route | Disposition | Where it goes |
|---|---|---|---|
| 15 | `GET /api/config/controllers` | **Dropped as a concept**, information preserved | Its contents are split across `GET /api/v1/services`, `GET /api/v1/website-server`, and `System → Advanced`. No screen lists "controllers". |
| 16 | `POST /api/config/controllers` | **Dropped** | Creating a service is now "turn on Weather Underground for this station", which is a `PUT /api/v1/services/wunderground`. There is no "add a controller" action. |
| 17 | `GET /api/config/controllers/{type}` | **Mapped**, split | `GET /api/v1/services/{service}` for `pwsweather`/`weatherunderground`/`aprs`/`aerisweather`/`snowcache`; `GET /api/v1/website-server` for `rest`; see below for `management`. |
| 18 | `PUT /api/config/controllers/{type}` | **Mapped**, split | Same split. |
| 19 | `DELETE /api/config/controllers/{type}` | **Mapped** as "turn off" | `DELETE /api/v1/services/{service}` — the screen says "Stop sharing with Weather Underground", and §2 shows the upload component actually stopping, which today it does not (`internal/managers/controller.go:148-152`: `RemoveController` deletes a map entry and logs "will stop on next app restart"). |

**`type == "management"` gets no page.** Today the management controller is editable through its own
API — port, listen address, certificate, and `auth_token` — which is how an operator locks themselves
out in one `PUT`. Under §3 the token no longer exists; the port, listen address and certificate move
to `System → Advanced`, where changing the port shows an explicit warning and the new URL, and the
listen address is governed by §3.5. This is a *narrowing*, stated as a decision rather than an
omission.

### 1.5 Website (5 + 2 registrations)

| # | Route | Disposition | Where it goes |
|---|---|---|---|
| 20 | `GET /api/config/websites` | **Mapped** | `GET /api/v1/websites` → **My website** |
| 21 | `POST /api/config/websites` | **Mapped** | `POST /api/v1/websites` |
| 22 | `GET /api/config/websites/{id}` | **Mapped** | `GET /api/v1/websites/{id}` |
| 23 | `PUT /api/config/websites/{id}` | **Mapped** | `PUT /api/v1/websites/{id}` |
| 24 | `DELETE /api/config/websites/{id}` | **Mapped** | `DELETE /api/v1/websites/{id}` |
| 25 | `POST /api/config/websites/{id}/radar/register` | **Mapped** | `POST /api/v1/websites/{id}/radar` → "Turn on the weather radar map" |
| 26 | `DELETE /api/config/websites/{id}/radar/register` | **Mapped** | `DELETE /api/v1/websites/{id}/radar` |

`RadarToken` is write-only and never returned (it is already "set only by registration",
`provider.go:412`); the UI shows **"Registered on 4 March 2026"** and a **Re-register** action, never
the token.

### 1.6 WeatherLink Live (6 registrations)

The discovery / template / validation family, which is genuinely good and under-used because it is
buried.

| # | Route | Disposition | Where it goes |
|---|---|---|---|
| 27 | `GET /api/weatherlink/discover` | **Mapped**, promoted | `GET /api/v1/discovery/weatherlink` — now runs **automatically** on the "add a station" screen (§0.7 step 2) instead of behind a button |
| 28 | `POST /api/weatherlink/validate` | **Mapped** | `POST /api/v1/discovery/weatherlink/validate` — inline validation of "which sensor is which" |
| 29 | `GET /api/weatherlink/templates` | **Mapped** | `GET /api/v1/discovery/weatherlink/templates` — the picker's options |
| 30 | `GET /api/weatherlink/template` | **Mapped** | `GET /api/v1/discovery/weatherlink/templates/{name}` |
| 31 | `GET /api/weatherlink/template/validate` | **Merged** into #28 | Two validation endpoints for one concept (one takes a mapping, one takes a template name); the new surface validates a *resolved mapping* and the template picker resolves client-side. |
| 32 | `GET /api/weatherlink/suggest` | **Mapped**, promoted | `GET /api/v1/discovery/weatherlink/suggest` — the suggestion is applied by default with "we guessed this from what your unit reports — change it if it's wrong", rather than offered as an action |

"Sensor mapping" and "template" never appear on screen; the section is **"Which sensor is which"**.

### 1.7 Diagnostics — the `/test/*` family (6 registrations)

~780 lines of genuinely useful diagnostics (`handlers_testing.go`). All six survive. W2a §4.4 makes
them `check:<name>` components with `Trigger::OnDemand`, run through `ComponentControl`, which is
exactly right: run when a human asks, not on a 90-second ticker forever.

| # | Route | Disposition | Where it goes |
|---|---|---|---|
| 33 | `POST /api/test/device` | **Mapped** | `POST /api/v1/checks/station` — also the **Test connection** button on the station form (§0.7) |
| 34 | `GET /api/test/database` | **Mapped** | `POST /api/v1/checks/database` — on the weather-database form and on `System → Diagnostics`. Becomes `POST` because it is an action against a remote system, not a cacheable read. |
| 35 | `GET /api/test/serial-port` | **Mapped** | `POST /api/v1/checks/serial-port` |
| 36 | `POST /api/test/api` | **Mapped** | `POST /api/v1/checks/web-endpoint` — "Can the outside world reach my website?" |
| 37 | `POST /api/test/current-reading` | **Mapped**, promoted | `POST /api/v1/checks/current-reading` — this is the engine behind §0.7 step 3 and the station page's live panel |
| 38 | `GET /api/test/storage` | **Mapped** | `POST /api/v1/checks/storage` |

**One check is added**, and it is not new work: W2a §3.2 observes that the APRS controller's
90-second health monitor (`aprs/controller.go:133-191`) is really a connectivity check wearing a
health hat, and makes it `check:aprs`. The UI exposes it as **Test** on the APRS page —
`POST /api/v1/checks/aprs`. Seven checks, one `ComponentControl` path, one results panel.

All checks share one response shape, so one component renders all seven:

```jsonc
{
  "ok": false,
  "duration_ms": 2041,
  "summary": "Couldn't reach the weather database.",       // plain, one line
  "detail": "connection refused",                           // technical, in the disclosure
  "causes": ["dial tcp 10.0.0.5:5432: connect: connection refused"],
  "hint": "Check that PostgreSQL is running and that port 5432 is open.",
  "fields": [ {"label": "Round trip", "value": "—"} ]       // check-specific extras
}
```

### 1.8 Logs (2 registrations)

| # | Route | Disposition | Where it goes |
|---|---|---|---|
| 39 | `GET /api/logs` | **Merged** | `GET /api/v1/logs?stream=app` plus the SSE channel. W3 §5.7 collapses the two rings into one API with a `stream` filter. |
| 40 | `GET /api/http-logs` | **Merged** | `GET /api/v1/logs?stream=access`. The *rings* stay separate (W3 §5.7's eviction-asymmetry argument holds); the *presentation* merges, because correlating a 500 with the query that failed is the whole job. |

### 1.9 System (2 registrations)

| # | Route | Disposition | Where it goes |
|---|---|---|---|
| 41 | `GET /api/system/serial-ports` | **Mapped** | `GET /api/v1/system/serial-ports` — feeds the serial-port picker (§0.7) |
| 42 | `GET /api/system/info` | **Mapped** | `GET /api/v1/system/info` → `System → This machine` |

### 1.10 Status, config and reload (4 registrations)

| # | Route | Disposition | Where it goes |
|---|---|---|---|
| 43 | `GET /api/status` | **Mapped**, rewritten | `GET /api/v1/runtime/components` + `GET /api/v1/runtime/summary`. Today's `/status` reports process uptime and a controller list; §2 replaces it with real supervised state. |
| 44 | `GET /api/config` | **Mapped**, narrowed | `GET /api/v1/config` under `System → Advanced` only, with every secret omitted. No ordinary screen fetches the whole configuration. |
| 45 | `GET /api/config/validate` | **Mapped**, rewritten | `POST /api/v1/config/preview` (§2.4) — it now validates *a proposed change* and returns the plan diff, rather than validating what is already saved. The old shape answers a question nobody asks. |
| 46 | `POST /api/config/reload` | **Dropped** | There is nothing to reload. A write through `ConfigAdmin` bumps the config watch and the supervisor reconciles (W2a §2.5: "nothing calls `ReloadConfiguration`"). Keeping a Reload button would re-teach the operator that saving does not take effect — which is precisely the lie §2 exists to remove. The screen affordance that replaces it is in §2.4. |

### 1.11 Credential rotation (1 registration)

| # | Route | Disposition | Where it goes |
|---|---|---|---|
| 47 | `POST /api/utils/change-token` | **Mapped**, replaced | `POST /api/v1/auth/password` (change password, requires the current one) and `DELETE /api/v1/auth/sessions` (sign out everywhere). The token it rotates does not exist under §3. |

### 1.12 The page and asset routes (12 registrations)

| # | Route | Disposition |
|---|---|---|
| 48–50 | `/css/*`, `/js/*`, `/images/*` | **Not UI-facing as API.** Replaced by the Vite build's hashed output under `/assets/*`, served by `rust-embed` (§6.4). |
| 51 | `GET /` | **Mapped** → Home |
| 52 | `GET /management` | **Dropped.** A second URL for the same page. The SPA serves one root. |
| 53 | `GET /weather-stations` | **Mapped** → My weather stations (`/stations`) |
| 54 | `GET /controllers` | **Dropped** → the word is retired (§0.2); the URL redirects to `/share` for one release |
| 55 | `GET /storage` | **Mapped** → `/data-storage` |
| 56 | `GET /websites` | **Mapped** → `/website` |
| 57 | `GET /logs` | **Mapped** → `/system/activity` |
| 58 | `GET /http-logs` | **Merged** into `/system/activity` with the stream filter (§1.8) |
| 59 | `GET /utilities` | **Dropped.** "Utilities" was the page for `change-token`; its contents move to `System → Administrator`. |

**SPA routing note.** All page routes are served by one `index.html` fallback, exactly as today
(`controller.go:258-268` registers nine paths to one handler). The new server registers one catch-all
instead of enumerating them, so adding a page is not a server change.

### 1.13 Two endpoints the shipped UI calls that do not exist

Found by reading the client rather than the router, which is why a route count cannot see them.

| Call site | Endpoint | Reality |
|---|---|---|
| `assets/js/management-api-service.js:337` | `GET /api/config/export` | **Registered nowhere in the repository.** Dead UI code calling a 404. |
| `assets/js/management-remote-stations.js:64` | `GET /api/remote-stations` | Registered on the **public REST server** (`internal/controllers/restserver/controller.go:532`), not on the management router. The management UI is served from the management port, so this relative fetch hits the management origin and 404s. |

**Disposition.** `config/export` is **dropped** — "export my configuration" is a real want, but the
answer is `System → Advanced → Download configuration` backed by `GET /api/v1/config` with secrets
omitted, not a resurrected dead endpoint. `remote-stations` is **mapped** to
`GET /api/v1/peers/stations` on the management API, under Data storage, and is W7's registry read
through `StationRegistry`. The existing public `/api/remote-stations` is part of the frozen contract
(W1) and is untouched.

### 1.14 Inventory summary

| Disposition | API routes | SPA page routes | Asset prefixes | Total |
|---|---|---|---|---|
| Mapped | 39 | 5 | — | **44** |
| Merged | 5 | 1 | — | **6** |
| Dropped | 3 | 3 | — | **6** |
| Not UI-facing | — | — | 3 | **3** |
| **Total** | **47** | **9** | **3** | **59** |

**59 server registrations**, plus the **2 client-only calls** in §1.13 (one mapped, one dropped).
Every one has a disposition.

**New routes this design requires** are in §2.6 and §4.3; all are additive, and none touches the
frozen public contract (W1, D-37).

---

## 2. Configured versus running

The most valuable thing this UI can gain, and the one thing it is incapable of today.

### 2.1 The three lies being removed

Each is measured, not inferred.

**Lie 1 — "Saved" means "applied".** `ReloadControllersConfig` builds `shouldBeActive` keyed by
controller *type*, removes types that are absent and adds types that are missing
(`internal/managers/controller.go:166-190`). It **never compares the contents of a controller's
configuration**. Change Weather Underground's upload interval, station ID, password or API endpoint
and press Save: the row is written, the reload runs, the type is still present, nothing is
reconstructed, and the old settings keep uploading until the process restarts. The UI says "saved"
and is telling the truth about the database and a lie about the system.

**Lie 2 — "Removed" means "stopped".** `RemoveController` cannot stop anything, because the
`Controller` interface has only a start method. It deletes a map entry and logs
`"Removed controller: %s (will stop on next app restart)"` (`internal/managers/controller.go:148-152`)
while the controller's goroutines and listeners keep running. The UI reports success.

**Lie 3 — a reload silently destroys an unrelated thing.** `ReloadStorageConfig` builds its
`shouldBeActive` set from `TimescaleDB` and `GRPC` only (`internal/managers/storage.go:158-166`).
`GRPCStream` is not in the set, so **every storage reload tears down the gRPC-stream uplink** — a
save on an unrelated page silently stops forwarding readings to a peer, with no screen reporting it.

**The design goal is not "warn about this". It is to make the class of lie unrepresentable.**

### 2.2 The mechanism: a configuration generation, carried end to end

One integer does most of the work.

- `ConfigAdmin` assigns a monotonic **`generation`** to every committed write.
- Every mutating response returns `{ "generation": 42, "affects": ["wu:backyard"], ... }`.
- Every component's runtime status carries **`applied_generation`** — the generation of the
  configuration the *running* instance was built from.
- The UI computes one derived fact: **a change is live when every affected component's
  `applied_generation ≥ generation`.** Until then it is *pending*, and the UI says so.

This is small, and it is sufficient, because it turns "did my change take effect?" from an
unanswerable question into an integer comparison the server cannot fake. A component that was not
restarted keeps its old `applied_generation` and is therefore visibly stale — exactly the condition
that is invisible today.

It also composes with W2a §2.5's plan diff: a save that changes nothing about a component leaves that
component untouched *and* bumps nothing for it, so "saving a website no longer disturbs the APRS
schedule" becomes observable rather than asserted.

### 2.3 The runtime status resource

```jsonc
// GET /api/v1/runtime/components
{
  "generation": 42,              // newest committed configuration
  "components": [
    {
      "id": "wu:backyard",                      // internal id; never rendered raw
      "display": {                              // the server does the translation, once
        "name": "Weather Underground",
        "qualifier": "Backyard",                // the station or website this belongs to
        "section": "share",                     // home | stations | share | website | storage | system
        "href": "/share/weather-underground"
      },
      "state": "waiting",
      "waiting_on": [
        { "provider": "db:postgres", "provider_display": "the weather database" }
      ],
      "degraded": [],
      "since": "2026-10-29T21:14:03Z",
      "applied_generation": 41,                 // ← 41 < 42: this change is NOT live
      "schedule": {
        "kind": "every", "period_s": 60,
        "last_run": "2026-10-29T21:13:02Z",
        "last_outcome": "ok",                   // ok | failed | skipped | deadline_exceeded
        "next_run": "2026-10-29T21:15:02Z"
      },
      "last_error": null,
      "counters": { "runs_total": 1182, "failed_total": 3, "skipped_total": 0,
                    "deadline_exceeded_total": 0, "restart_total": 0 },
      "spool": null,
      "freshness": null
    }
  ]
}
```

Four things about this shape are deliberate.

**The server renders the vocabulary, not the client.** `display.name` / `display.qualifier` /
`display.section` come from the server. If translation lived in the UI, every new component would
need a UI change to be nameable, and the §6.6 lint could not see the mapping. The §0.2 table is
therefore implemented **once**, in `mw-api-mgmt`, and the UI renders what it is given. The internal
`id` travels for keying, deep-links and support, and is rendered only inside `System → Advanced`.

**`applied_generation` is per component, not global.** A global "configuration version applied" would
be wrong the moment one component restarts and another does not — which is the common case, since
unchanged components are deliberately not touched.

**`waiting_on` carries the provider, not just the capability.** This is the thing W2a §3.1 added for
this document — "waiting on `ReadingStore`" is unactionable, "waiting for the weather database" is a
sentence an operator can act on.

**Schedule state is real, not inferred.** W2a §1.3 puts the clock in the supervisor, so `last_run`,
`last_outcome` and `next_run` are state rather than a guess. Today they are local variables inside
`for { select { case <-ticker.C: } }` loops and are not observable at all.

### 2.4 "Accepted but not yet in effect" — the screen affordance

chonky ships exactly the right component: `ApplyBanner { count: number, onApply: () => void }`. It is
used, but **not with the semantics its name suggests**, and the difference matters.

> **Saving applies.** There is no "pending changes" staging area and no Apply button that the
> operator must find. A `PUT` is committed, the supervisor reconciles, and the UI reports the
> reconciliation. The banner reports *progress*, not *a queue*.

Why not a staging model, since chonky offers one: a staged edit that the operator forgets to apply is
the same defect as today's silent no-op, reached by a different road. Today the lie is "saved means
running"; a staging model's lie is "I thought I saved that". The honest shape is to save immediately
and show the system catching up.

The three states, in order:

```
① BEFORE SAVE — the preview, on the form's Save button
   ┌──────────────────────────────────────────────────────────────┐
   │ Saving will restart the Weather Underground upload for       │
   │ Backyard. Nothing else is affected.         [ Cancel ][ Save ]│
   └──────────────────────────────────────────────────────────────┘

② JUST SAVED — ApplyBanner, count = components still catching up
   ┌──────────────────────────────────────────────────────────────┐
   │ ⟳ Saved. 1 item is restarting…                               │
   └──────────────────────────────────────────────────────────────┘

③ LIVE — banner replaced, holds 3 s, then fades
   ┌──────────────────────────────────────────────────────────────┐
   │ ✓ Saved and live.                                            │
   └──────────────────────────────────────────────────────────────┘
```

And the two failure paths, which are the point of the whole mechanism:

```
④ STILL NOT LIVE AFTER 10 s
   ┌───────────────────────────────────────────────────────────────┐
   │ ⚠ Saved, but not running yet. The Weather Underground upload  │
   │   is waiting for the weather database to connect.             │
   │   Your change will take effect automatically. [ What's wrong? ]│
   └───────────────────────────────────────────────────────────────┘

⑤ RESTARTED AND FAILED
   ┌───────────────────────────────────────────────────────────────┐
   │ ✗ Saved, but it won't start: Weather Underground rejected the │
   │   station ID.                        [ Details ][ Undo save ] │
   └───────────────────────────────────────────────────────────────┘
```

**The preview in ① is not cosmetic.** It is `POST /api/v1/config/preview`, which runs the supervisor's
`plan()` against the proposed configuration and returns the diff without committing. W2a §2.5
establishes that `plan()` is pure and that an invalid configuration is rejected before anything is
stopped — so a dry run is free, and the operator learns "this restarts your APRS beacon" *before*
agreeing to it rather than discovering it afterwards.

```jsonc
// POST /api/v1/config/preview   { "stations": [ ... ] }   → 200
{
  "valid": true,
  "diff": {
    "start":     [],
    "stop":      [],
    "restart":   [ {"id":"wu:backyard","display":"Weather Underground (Backyard)"} ],
    "unchanged": 27
  },
  "warnings": [
    "Weather Underground asks for no more than one upload a minute; 30 seconds may be rejected."
  ]
}
// → 422 when plan() rejects it
{ "valid": false, "errors": [
    { "field": "stations[0].hostname", "message": "Enter a hostname or IP address." } ] }
```

**`Undo save` in ⑤ is a real action, not a hope.** Because every write has a generation, the server
keeps the previous committed configuration for the current session and
`POST /api/v1/config/revert { "to_generation": 41 }` restores it. Scope is deliberately small: one
step back, within the signed-in session, offered only on the failure banner. Full configuration
history is **OQ-W5-5**.

### 2.5 The dependency view — one root cause, not five outages

W2a §3.4's worked example: when the database becomes unreachable, `writer:timescale`,
`window:rollup`, `aprs:*`, `pws:*` and `wu:*` all move to `Waiting`. An operator must see **one
problem**, not five.

The server does the graph traversal, because the UI reimplementing it would be a second
implementation of the supervisor's own topology:

```jsonc
// GET /api/v1/runtime/summary
{
  "generation": 42,
  "health": "degraded",                  // ok | degraded | broken
  "root_causes": [
    {
      "id": "db:postgres",
      "display": "the weather database",
      "state": "backoff",
      "message": "Can't connect to the weather database. Retrying in 12 seconds.",
      "detail": "connection refused",
      "since": "2026-10-29T21:12:40Z",
      "blocking": [
        {"display":"Saving readings",                 "href":"/data-storage"},
        {"display":"Recent-conditions calculations",  "href":"/system/diagnostics"},
        {"display":"Weather Underground (Backyard)",  "href":"/share/weather-underground"},
        {"display":"PWS Weather (Backyard)",          "href":"/share/pws-weather"},
        {"display":"APRS / CWOP (Backyard)",          "href":"/share/aprs"}
      ],
      "unaffected_note": "Your website and the phone app are still working."
    }
  ],
  "counts": { "running": 24, "waiting": 5, "failed": 1, "backoff": 1, "off": 3 }
}
```

Rendered on Home:

```
┌─ Something needs attention ─────────────────────────────────────────┐
│ ✗ Can't connect to the weather database.                            │
│   Retrying in 12 seconds.                            [ Details ]    │
│                                                                      │
│   Because of this, 5 things are waiting:                             │
│     Saving readings · Recent-conditions calculations ·               │
│     Weather Underground (Backyard) · PWS Weather (Backyard) ·        │
│     APRS / CWOP (Backyard)                                           │
│                                                                      │
│   Your website and the phone app are still working.                  │
└──────────────────────────────────────────────────────────────────────┘
```

The last line is not reassurance padding — it is W3 §2a.6's measured result (`/latest` is served from
the in-memory snapshot and is untouched by a database outage) and it is exactly the thing an operator
cannot currently determine during an incident.

**A blocked component's own page shows the blockage, not an error.** The Weather Underground page
during this outage shows the **Waiting** badge and "Can't start yet — waiting for the weather
database to connect. [Go to the weather database]", with its settings still editable and its save
path still working. It does not show a red failure, because it has not failed.

### 2.6 New endpoints this section requires

All additive, all under the management API, none part of the frozen contract (W1, D-37).

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/v1/runtime/summary` | Root-cause rollup + counts (§2.5) |
| `GET` | `/api/v1/runtime/components` | Full status list (§2.3) |
| `GET` | `/api/v1/runtime/components/{id}` | One component |
| `POST` | `/api/v1/runtime/components/{id}/run-now` | `ComponentControl` run-now, for `Every` / `Calendar` / `OnDemand` |
| `POST` | `/api/v1/runtime/components/{id}/restart` | Cancel and re-create |
| `POST` | `/api/v1/config/preview` | Pure `plan()` dry run → diff (§2.4) |
| `POST` | `/api/v1/config/revert` | One step back (§2.4) |
| `GET` | `/api/v1/events` | SSE: component-state, log and reading channels (§5) |
| `POST` | `/api/v1/runtime/log-level` | `set_level(target, level, ttl)` (§5.5) |
| `GET` | `/api/v1/runtime/log-levels` | Standing overrides (§5.5) |

**Start/stop is deliberately *not* on this list** as a general capability. `run-now` and `restart` are
safe and reversible; a general "stop this component" button creates a running state that no
configuration describes, which is a second source of truth and the thing D-28 and W2a §2.5 exist to
remove. Turning something off is a **configuration** change (the station's "Collecting readings"
toggle, "Stop sharing with Weather Underground"), which persists, is visible on the page that owns
it, and survives a restart. Recorded as **Decision 22**.

---

## 3. Authentication — username and password

One administrator. No RBAC, no user table, no roles, no permissions (D-35). Every requirement below
is scoped to the **management** API; the public API is unauthenticated and frozen by W1, so there is
no contract risk anywhere in this section.

### 3.1 What is being replaced, verified

| Defect | Evidence |
|---|---|
| A UUID used as a bearer token, with no username at all | `Login` decodes `{ "token": ... }` only — `handlers.go:227-246` |
| Compared with `==`, not constant-time | `request.Token != h.controller.managementConfig.AuthToken` — `handlers.go:243` |
| Logged in cleartext on every failed attempt | `c.logger.Debugf("Bearer token mismatch: got %s, expected %s", ...)` — `controller.go:334` |
| …including into the in-memory buffer this very UI displays | the same logger feeds `/api/logs` |
| Session cookie set to the token value itself | `Value: request.Token` — `handlers.go:250` |
| …so a browser session cannot be revoked without rotating the API credential | corollary; `Logout` can only clear the client's copy — `handlers.go:268-285` |
| `Access-Control-Allow-Origin: *` on the admin API | `controller.go:306` |
| Default bind `0.0.0.0` | `DefaultListenAddr` default — `pkg/config/provider.go` |
| The credential is editable through the API it protects | `PUT /api/config/controllers/management` carries `auth_token` |

### 3.2 Credential storage

- **Argon2id**, via the `argon2` crate (D-32). Store the **full PHC string**
  (`$argon2id$v=19$m=65536,t=3,p=4$<salt>$<hash>`) so the parameters travel with the hash.
- **Parameters:** m=64 MiB, t=3, p=4 as the policy floor. This is a deliberate choice for the target
  hardware: the deployment runs on Raspberry-Pi-class machines (W8 builds armv6/armv7/aarch64), and
  64 MiB for one concurrent login is affordable on the 512 MB target W3 §5.7 already sizes against,
  while 256 MiB would not be. Login is a once-a-week operation for one person.
- **Transparent upgrade on login.** Verify against the stored PHC parameters; if they are below
  current policy, re-hash the just-verified plaintext with current parameters and replace the row,
  inside the same transaction that records the successful login. This is the whole reason for storing
  PHC rather than a bare digest.
- Never store the password. Never log it. **No endpoint returns it, and no endpoint returns the
  hash** — including `GET /api/v1/config` in the Advanced view, where the field is *absent* rather
  than redacted to a placeholder.
- The password is wrapped in `secrecy::SecretString` (D-33) from the moment it is deserialised, so a
  stray `{:?}` cannot print it.

### 3.3 Sessions are not credentials

This is the specific defect from §3.1 and it gets a specific structure.

```sql
-- SQLite, config database
CREATE TABLE admin_session (
  id            BLOB PRIMARY KEY,   -- 16 random bytes; the public handle, safe to log
  token_hash    BLOB NOT NULL,      -- SHA-256 of the 32-byte secret. The secret is never stored.
  created_at    INTEGER NOT NULL,
  last_seen_at  INTEGER NOT NULL,
  expires_at    INTEGER NOT NULL,
  user_agent    TEXT,               -- for "which browser is this?" in the session list
  created_ip    TEXT
);
```

- **Login** (`POST /api/v1/auth/login`, `{username, password}`) verifies with Argon2id, mints a fresh
  **32-byte random** session secret from the OS CSPRNG, stores `(id, SHA-256(secret))`, and returns
  the cookie `mw_session=<id>.<secret>`.
- **Cookie attributes:** `HttpOnly`, `SameSite=Strict`, `Path=/`, `Secure` **whenever the request
  arrived over TLS**, `Max-Age` = the session lifetime, and the `__Host-` prefix when `Secure`
  applies.
- **Lifetime:** 14 days absolute, 7 days idle. Both are generous because this is one operator on
  their own network and a short expiry buys nothing but friction; both are configurable under
  `System → Administrator`.
- **Hashing the session secret is not ceremony.** A read of the config SQLite file — a backup, a
  support dump, an `scp` — must not hand over a live session. SHA-256 is correct rather than Argon2id
  here because the secret is 32 random bytes, so there is nothing to brute-force; the slow hash is
  only needed for low-entropy human input.
- **Comparison is constant-time** (`subtle`, D-33) on the token hash.
- **Individually revocable:** `GET /api/v1/auth/sessions` lists `{id, created_at, last_seen_at,
  user_agent, current}`; `DELETE /api/v1/auth/sessions/{id}` revokes one;
  `DELETE /api/v1/auth/sessions` is **"Sign out everywhere"** and deletes every row including the
  current one.
- **Changing the password revokes every other session** and keeps the current one, which is what a
  person expects after "my laptop was stolen".
- **CSRF.** `SameSite=Strict` is the primary defence. Because the management UI is a same-origin SPA
  it costs nothing to add a second: every state-changing request must carry `X-MW-Request: 1`, a
  header a cross-site form post cannot set without a CORS preflight that §3.5's allowlist refuses.
  Both, not one — `SameSite` has historically had parser-dependent gaps, and a custom header is three
  lines.

### 3.4 Initial password setup

The flow most likely to be got wrong, specified so it is not simplified away.

**Primary path — a one-time setup claim.**

1. On start with no administrator row, generate a **single-use setup token**: 32 random bytes,
   base32-encoded without ambiguous characters — this gets read off a console and retyped — stored as
   a hash.
2. Print it **once**, to stderr, clearly marked:

   ```
   ══════════════════════════════════════════════════════════════════════
    RemoteWeather is not set up yet.
    Open  http://<this-host>:8081/  and enter this one-time setup code:

        K7QF-2MXB-9HTD-4RWP-6JSA-1NVC-8YEK-3LZU

    This code is shown once and is valid until an account is created.
    Anyone with this code can create the administrator account.
   ══════════════════════════════════════════════════════════════════════
   ```

3. The UI's first screen is **"Create your administrator account"**, requiring the code, a username
   and a password (twice).
4. The token is invalidated the moment the account row exists — in the same transaction, so a race
   between two claimants resolves to exactly one.
5. The token is `mw::audit`-logged as *issued* (not its value), and the successful claim is logged
   with its source IP.

**The hazard, stated because it is the reason the code exists.** An unclaimed instance bound to
`0.0.0.0` means **whoever reaches the setup page first owns the install**. This is a real
vulnerability in naive first-run flows, not a theoretical one: the window is "from first start until
the owner gets round to it", which on a headless Pi can be days, and the attacker needs nothing but
network reachability. Requiring the setup code closes it, because only someone with console or
service-log access has the code — the same trust boundary as §3.6's recovery path.

> **This paragraph exists so that a future "let's make first-run friendlier" change has to argue with
> it.** Removing the code to save the operator one paste re-opens a remote account takeover.

**Binding to loopback until claimed — recommended, with the friction acknowledged.**

The management API's `listen_addr` is **`127.0.0.1` while unclaimed**, widening to the configured
value once an administrator exists. Reasoning: it makes the takeover window *empty* rather than
merely gated, and it costs nothing in the common case, because the common case is a person sitting at
or SSH'd into the machine.

The friction is real and must be handled rather than waved at: a headless remote install then cannot
reach the setup page at all. Three answers, in order of preference, all offered in the console banner:

1. **An SSH tunnel** — `ssh -L 8081:localhost:8081 pi@weatherbox`, printed verbatim in the banner with
   the host's own name substituted. One command, and most remote operators already have the session
   open.
2. **`--setup-listen 0.0.0.0`** — an explicit opt-out that widens the unclaimed bind. It logs a `WARN`
   naming the risk, and it is explicit, which is the property that matters.
3. **The automation path below**, which skips the web claim entirely.

Recorded as **Decision 29** and raised as **OQ-W5-1**, because the friction judgement is the owner's.

**Automation path — optional, with the trade-off stated plainly.**

`MW_INITIAL_ADMIN_USERNAME` / `MW_INITIAL_ADMIN_PASSWORD` (or `MW_INITIAL_ADMIN_PASSWORD_FILE`,
preferred), consumed **once** at first start when no administrator exists, and ignored thereafter. For
Docker and provisioning. The trade-off, stated in the documentation and in the startup log line: **an
environment variable ends up in shell history, in the compose file, in `docker inspect`, and in
`/proc/<pid>/environ`.** The `_FILE` variant is preferred because a Docker secret or a mode-0600 file
is readable by fewer things, and the daemon reads it once and does not retain it.

**Never a default password. Never an empty one.** There is no built-in account, no `admin/admin`, and
no "unauthenticated until you configure auth" mode. An instance is either unclaimed — and therefore
useless except for being claimed — or it has a real credential.

### 3.5 Proportionate hardening

One operator on a trusted network, so this stays modest. Everything here is cheap, and omitting any of
it would be a defect rather than a simplification.

- **Progressive delay on failed logins.** Per-username, not per-IP (there is one username, and per-IP
  is trivially evaded): attempts 1–3 free, then `min(2^(n-3), 30)` seconds, reset on success or after
  15 minutes of quiet. A hard lockout is deliberately **not** used — it is a self-inflicted denial of
  service against an account with no second administrator to unlock it. The delay is applied *before*
  Argon2 verification so it cannot be used as a timing oracle.
- **Minimum length 12, no composition rules.** Current NIST guidance (SP 800-63B) deprecates mandatory
  character-class requirements; they push people toward `Password1!` and buy nothing. A **maximum** of
  256 bytes exists only to bound Argon2's input. The strength meter shown is an estimate, not a gate.
- **Check against a breached-password list?** **OQ-W5-2** — the one item here whose cost is not
  obviously below its value for a single-operator LAN tool.
- **Changing the password requires the current password**, rate-limited on the same schedule as login.
- **Constant-time comparison** (`subtle`, D-33) for the session token hash, the setup token hash, and
  any other secret comparison. Argon2 verification is already constant-time.
- **Nothing secret in logs.** The one deliberate exception is the setup token, printed once to the
  console by design — and it goes **to stderr only, never to the log ring**, because the UI that
  displays the ring is the surface that must not leak it. Authentication outcomes are logged on
  `target = "mw::audit"` (W3 §5.6: never suppressed) as `{outcome, username, source_ip, session_id}`,
  with no credential material of any kind.
- **CORS: an allowlist, replacing `*`.** Default empty, meaning same-origin only, which is correct for
  an embedded SPA. A configurable allowlist exists under `System → Advanced` for the operator who
  genuinely runs the UI from another origin, and it is an exact-match list of origins — never `*`,
  never a reflected `Origin`.
- **Default bind address: reconsidered, and changed.** The management API's `listen_addr` default
  becomes **`127.0.0.1`** rather than `0.0.0.0`. The operator who wants LAN access sets it once and
  deliberately; the operator who does not, never exposes an admin surface by accident. This is a
  default change for the *management* API only — the public website's bind is W3's and is untouched —
  and it is **Decision 30** because it changes installed behaviour.
- **Security headers** on every management response: `Content-Security-Policy:
  default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'` (the SPA has no
  inline script and no CDN, so no `unsafe-inline` is needed), `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`.

### 3.6 Recovery — a CLI subcommand

```
modernweather reset-password [--username <name>]
```

Run on the host. Prompts twice on a TTY, writes a new Argon2id hash, and **revokes every session**.
Non-interactive use reads from stdin so it can be scripted.

**The trust boundary is local filesystem access, and that is the right one.** If you can run this, you
can already read the config SQLite file, which contains the database password, the Weather Underground
password and the APRS passcode. A recovery path weaker than the thing it protects would be a hole; one
that is stronger would be theatre. This is exactly as strong.

It is also what makes "never ship a default or backdoor credential" affordable — the locked-out case
has an answer that does not require one.

Two companions, same boundary:

- `modernweather reset-setup-token` — re-issues the one-time claim code for an instance that was never
  claimed and whose console output has scrolled away.
- `modernweather sessions revoke --all` — for "I left myself signed in somewhere".

### 3.7 API tokens — recommendation: none in v1

**The measured situation:** the iOS app uses the *public* API, which is unauthenticated
(`docs/legacy-api-contract.md`), and the management UI is a same-origin browser client. **Nothing
currently requires a programmatic credential for the admin API.** The only consumer of today's bearer
token is a human pasting a UUID into a login box, which is a password with extra steps.

**Recommendation: omit API tokens from v1.** Build the one-administrator session model, ship it, and
add tokens when something actually needs one. The cost of adding a well-designed token later is one
table and three endpoints; the cost of shipping one speculatively is a second authentication path to
keep correct forever.

If the owner wants one anyway (**OQ-W5-3**), the shape is fixed in advance so it cannot regress into
today's design:

- **Explicitly created**, never implicit, never derived from the password or the session.
- **Shown exactly once** on creation, stored as a SHA-256 hash, never retrievable.
- **Named and revocable**, with `last_used_at`, so a forgotten token is visible.
- Presented as `Authorization: Bearer mw_<id>_<secret>` — a prefix that is greppable in logs and
  recognisable to secret scanners.
- **Never set as a cookie, and the session cookie is never accepted as a bearer token.** Two
  credential types, two verification paths. Re-introducing "the session cookie is the API token" is
  the exact defect being removed.

---

## 4. API contract — a generated client, and drift as a build failure

### 4.1 Where the schema comes from

Two schemas, one pipeline, and they must not be confused:

| Schema | Owner | Contents |
|---|---|---|
| `generated/openapi/reading.json` | **W2's generator**, from `metrics/registry.toml` | `Reading`, `LegacyWeatherReading` — the *data* |
| `generated/openapi/management.json` | **this design**, from the management handlers | The management API — the *operations* |

They are different artefacts with different sources of truth, and the management schema
**references** the reading schema rather than redefining it (`$ref` into the generated component
schemas), so a new metric reaches the UI's types without the management API being touched.

- **The management schema is derived from the Rust handlers**, using `utoipa` derives on the axum
  handlers in `mw-api-mgmt`, emitted by `cargo xtask codegen` into
  `generated/openapi/management.json` and committed. Hand-writing the schema separately from the
  handlers guarantees eventual divergence; deriving it means the handler *is* the schema.
- The UI's metric labels and units come from **`generated/ts/metrics.ts`** (W2), not from a
  hand-maintained list — W2's assumption 18, honoured. §0's vocabulary table is authoritative where
  it and a generated `label` disagree (W2 Decision 32), and the mechanism for that is §6.6's copy
  module overriding a specific key, never editing the generated file.

### 4.2 How the client is generated and wired in

```
metrics/registry.toml ──┐
                        ├─▶ cargo xtask codegen ─▶ generated/openapi/reading.json     (W2)
crates/mw-api-mgmt ─────┘                       ─▶ generated/openapi/management.json  (W5)
                                                        │
                                                        ▼
                                              openapi-typescript
                                                        │
                                                        ▼
                                        generated/ts/mgmt-api.d.ts   (committed)
                                                        │
                                                        ▼
                               ui/src/lib/api.ts  —  openapi-fetch typed client
```

- **`openapi-typescript` + `openapi-fetch`**, not a heavyweight generator. `openapi-typescript` emits
  **types only** — no runtime, no classes, no generated method per endpoint — and `openapi-fetch` is
  a ~2 KB wrapper that applies those types to a `fetch` call. The whole client contributes about 2 KB
  to the bundle rather than tens of kilobytes of generated classes, which matters against §6.5's
  budget.
- The generated `.d.ts` is **committed**, carries a do-not-edit header, and is regenerated by the
  same `cargo xtask codegen` invocation as everything else, so there is one command and one drift
  gate rather than two.
- **Nothing hand-written lives in `generated/`** (W3 §1.6 rule 3). The typed client wrapper, the
  vocabulary mapping and any convenience helpers live in `ui/src/lib/`.

### 4.3 Drift becomes a build failure, by two independent mechanisms

Belt and braces, because they fail differently.

**Mechanism 1 — schema drift (CI, fast).** `cargo xtask gate codegen-drift` already exists for the
metric registry (`.github/workflows/pr.yml`) and regenerates into a temporary directory and diffs
(R-2). The management schema and the `.d.ts` join the same gate. A handler whose signature changed
without the schema being regenerated fails the PR.

**Mechanism 2 — type drift (build, exact).** The UI build runs `svelte-check` / `tsc --noEmit`
against the committed `.d.ts`. A UI that reads `component.applied_generation` after the server
renamed the field does not compile. This is the mechanism that turns a **runtime 404 or a silent
`undefined` into a build failure**, which is what the prompt asks for.

Mechanism 1 catches "the schema is stale". Mechanism 2 catches "the UI is stale". Neither catches the
other's case, which is why there are two.

**Paths are not string literals in the UI.** `openapi-fetch` is parameterised by the schema's path
union, so `api.GET('/api/v1/runtime/componnts')` is a type error, not a 404 at 2 a.m.

**What is deliberately not covered.** Neither mechanism catches a *semantic* change behind an
unchanged signature — a field that keeps its type and changes its meaning. That is what §6.7's
handful of end-to-end tests against a real binary are for, and it is stated so nobody believes the
type system is doing more than it is.

### 4.4 Designing against a schema that does not exist yet

Per the prompt, this design does not block on W2's artefacts or on `mw-api-mgmt` existing. It
specifies **shapes**, and the shapes are conservative in one specific way: **every new resource is a
JSON object with named fields, never a bare array or a scalar**, so a field can be added without a
version bump. The paths are versioned (`/api/v1/`) so a breaking change has somewhere to go, and the
versioning is on the **management** API only — the frozen public contract is unversioned by
definition and is not touched (D-37).

---

## 5. Live data — SSE

The current UI polls. W3 §5 and W2a §4.4 (`live:stream`, `Trigger::OnReading`) establish push, and the
management UI uses the same mechanism.

### 5.1 One endpoint, several channels

```
GET /api/v1/events?channels=components,logs,readings
                  &log_stream=app,access&log_level=info
Accept: text/event-stream
```

One connection, typed by the SSE `event:` field: `component`, `log`, `reading`, `gap`, `heartbeat`.

**Why one endpoint rather than one per channel.** HTTP/1.1 caps a browser at six connections per
origin, and this UI is served from a Rust binary that may well be on plaintext HTTP/1.1 on a LAN. Six
is enough until the operator opens a second tab. More importantly, one connection means **one
reconnect path, one backfill path and one gap story** instead of three that drift apart.

Channel subscription is a query parameter and is also adjustable in-stream
(`POST /api/v1/events/{stream_id}/channels`), so navigating from Home to the activity log does not
drop and re-establish the connection.

### 5.2 Reconnect

- **`Last-Event-ID` is honoured.** Every event carries a monotonic `id:`. On reconnect the browser's
  `EventSource` sends `Last-Event-ID` automatically; the server replays from the ring after that id,
  or emits a `gap` event if the id has already been evicted.
- **Backoff:** 1 s, 2 s, 4 s, 8 s, capped at 15 s, with ±20% jitter. Capped low because this is a LAN
  tool watched by a person: a 60 s cap would mean a UI that looks dead for a minute after a daemon
  restart, which is exactly when the operator is watching.
- **The UI says what it is doing.** The `StatusBar` shows `● live` / `◌ reconnecting (3s)` /
  `○ disconnected — [Retry]`. It never silently shows stale data as though it were live: any panel fed
  by the stream dims and shows "last updated {time}" while disconnected.
- **A server restart is not an error.** The daemon restarting mid-stream produces one reconnect, a
  `gap` if applicable, and a single `INFO`-level line in the activity view — not a red banner. The
  distinction is `generation` moving forward (an intended restart) versus the connection simply
  failing.
- **Heartbeat:** a comment frame every 20 s, so an idle proxy does not reap the connection and a
  black-holed connection is detected in bounded time.

### 5.3 Backfill on connect

W3 §5.7: "backfill on connect is the ring's current contents, newest-last, followed by live records —
one code path, because the ring and the live stream are the same structure." The UI consumes exactly
that.

- Default backfill: the **last 500 records**, honouring the requested filters, newest-last. Full-ring
  retrieval (4,096 app + 16,384 access) is a paged `GET /api/v1/logs`, not the stream.
- The boundary between backfill and live is explicit: a `{"kind":"caught_up"}` marker, so the UI can
  render "— live —" at the right place instead of guessing.
- Component state backfills as a **full snapshot** (every component's current state) followed by
  deltas, so the UI never has to reconstruct state from a partial event history.

### 5.4 Overflow

W3 §5.7 defines a synthetic record `{"kind":"gap","stream":"access","dropped":412}`. Rendered as an
inline marker in the log list — not a toast, and not a silent skip:

```
  21:14:02  INFO   station:backyard   reading accepted
  ────────  412 entries dropped (the log filled up faster than it could be shown)  ────────
  21:14:09  WARN   db:postgres        connection lost
```

If gaps occur repeatedly within a minute, the view adds a one-line explanation above the list — "The
log is producing entries faster than this view can keep up. Narrowing the filter will help." —
because an operator seeing three gap markers deserves to know it is a volume problem and not data
loss in the system itself.

### 5.5 Client-side filtering, and runtime log levels

**Two filters, deliberately distinct, because conflating them is how operators lose data:**

| | Where | Effect |
|---|---|---|
| **View filter** | client-side, over what is already buffered | instant, non-destructive, changes nothing on the server |
| **Capture level** | `set_level(target, level, ttl)` on the server | changes what is *recorded* — can destroy detail |

The view filter is the default interaction: level (`ERROR`/`WARN`/`INFO`/`DEBUG`/`TRACE`), stream
(`app`/`access`/both), station, and a free-text match over message and fields. It is applied to the
client's buffer and to the SSE subscription's server-side filter simultaneously, so narrowing also
reduces what crosses the wire.

**Runtime level control** is W3 §5.5's `set_level(target, level, ttl)`, exposed as a first-class
feature because "raise `mw_station::davis` to DEBUG for ten minutes" is genuinely useful and currently
impossible:

```
┌─ Show more detail ──────────────────────────────────────────────────────────┐
│  For            [ Davis station — Backyard                  ▾ ]             │
│  Detail         [ Everything (DEBUG)                        ▾ ]             │
│  For how long   [ 10 minutes ▾ ]  5 / 10 / 30 min · 1 h · Until I turn it off│
│                                                            [ Turn on ]      │
└──────────────────────────────────────────────────────────────────────────────┘
```

- The **target picker lists plain names**, not `tracing` targets. "Davis station — Backyard" maps to
  `mw_station::davis[station="backyard"]` server-side, by the same §2.3 mechanism that renders
  component names. A free-text target field exists in `System → Advanced` for someone who knows what
  they want.
- **A TTL is the default and is pre-selected** (10 minutes). W3 §5.5 records the expiry in the
  supervisor's heap, so it reverts without anybody remembering.
- **A TTL-less override is allowed and is shown as a standing condition** — a persistent amber chip in
  the activity view's toolbar, "Showing extra detail for Davis station — Backyard. [Stop]", and a line
  on `System → Advanced`. A debug level left on forever is a condition an operator should see stated,
  not a setting that quietly fills the ring with noise and evicts the error they are hunting.
- `GET /api/v1/runtime/log-levels` lists active overrides with their expiries.
- **The per-request `RW-Debug: 1` escalation** (W3 §5.5, frozen by W1) is *not* exposed as a UI
  control. It is a frozen-contract client behaviour, and the UI's equivalent is the scoped raise
  above.

### 5.6 Where live data appears

| Surface | Channel | Content |
|---|---|---|
| Home — attention panel | `components` | Root-cause rollup, live |
| Home — station tiles | `readings` | Latest reading per station, "2s ago" |
| Station detail — live panel | `readings` | §0.7 step 3's panel |
| Any page's ApplyBanner | `components` | `applied_generation` catching up (§2.4) |
| Activity log | `logs` | The merged ring |
| Every status badge, everywhere | `components` | No page polls for status |

**No page polls.** The two 3-second polling loops the current system runs
(`restserver/controller.go:1056`, `weatherapps_grpc.go:128`) are deleted by W3; the management UI must
not reintroduce the pattern. One exception, stated so it is not treated as a violation: the
`Backoff { until }` countdown ticks on a client-side timer against a server-supplied absolute
timestamp. That is rendering, not polling.

---

## 6. Technology — Svelte 5 on `chonky`

**Settled; not re-evaluated.** Owner decision: Svelte, and the owner's own toolkit
[`chonky`](https://github.com/chrissnell/chonky). The job here is to use it well.

Inspected at `f595bae` (`@chrissnell/chonky-ui` v0.4.0).

### 6.1 What `chonky` actually provides

| | |
|---|---|
| Framework | **Svelte 5** (runes: `$props`, `$state`, `$derived`), `bits-ui` ^2 for headless primitives |
| Peers | `bits-ui`, `@internationalized/date`, `lucide-svelte`, `svelte` ^5 |
| Components | **60 directories**, 61 value exports in `src/lib/index.ts` (the README's "56" is stale) |
| Compound pattern | `Component.Root` / `.Sub` — `Tabs.Root`/`List`/`Trigger`/`Content`, `Modal.Root`/`Header`/`Body`/`Footer` |
| Theming | CSS custom properties in `tokens.css`; `data-theme="dark"` on `<html>`; `ThemeToggle` persists to localStorage; OS preference honoured before JS runs |
| Design language | Monospace throughout (Inconsolata), 1px solid borders, `--radius: 2px`, **no shadows, no gradients** |
| Icons | `Icon` with a curated **38-icon** Lucide allowlist, keyed by kebab-case name |
| Build conventions | `svelte-package`, Vite 6, pnpm workspace; the owner's own docs site is **SvelteKit 2 + `adapter-static` 3 + Vite 6** |

Components unusually well-suited to this application, which shape the design rather than merely
decorating it:

- **`LogViewer`** — purpose-built for this: `entries`, `columns` (with per-column `render` snippets),
  `live`, `autoscroll`, `height`, `toolbarToggles` (pause / auto-scroll switches in the toolbar), a
  per-entry `footer` snippet, and a `card`/`scroll` mobile layout below a configurable breakpoint. The
  activity view is this component plus the SSE stream.
- **`ApplyBanner`** — `{ count, onApply }`. §2.4 uses it for the reconciliation banner.
- **`EmptyState`**, **`StatusBar`**, **`Dot`**, **`StatCard`** (6 variants), **`Badge`** (5 variants),
  **`Meter`** (`low`/`high`/`invert` thresholds) — the operational vocabulary is already there.
- **`Command`** — a command palette, a good fit for an admin tool with twenty-odd pages.

### 6.2 Every screen mapped to `chonky` components

| Screen / element | `chonky` |
|---|---|
| App shell, sidebar nav | **Local** — see gap G1 |
| Mobile nav | `Drawer` + local nav list |
| Section header, breadcrumbs | `BoxHeader`, `Breadcrumb` |
| Page sections | `Box` (`title` prop), `Separator` |
| Home: counts (running / waiting / failed) | `StatCard` (`success` / `warning` / `danger`) |
| Home: attention panel | `Box` + `Badge` + `Button` |
| Home: first-run checklist | `Box` + `Icon check` + `Button`; **local** step list |
| Station list, website list, session list | `Table` + `Badge` + `DropdownMenu` (row actions) |
| Status badge (every lifecycle state) | `Badge` + `Dot` — see gaps G2, G3 |
| Live indicator | `Dot` + `StatusBar` |
| Forms | `Label` + `Input` / `Select` / `Toggle` / `Checkbox` / `Slider` — see gap G4 |
| Station type chooser | `RadioGroup` |
| Serial-port picker, time-zone picker | `Combobox` (both are long and searchable) |
| Secret fields (passwords, passcodes, API keys) | `Input type="password"` + `Icon eye` / `eye-off` |
| Destructive confirmation | `AlertDialog` |
| Edit / add dialogs | `Modal.Root` / `Header` / `Body` / `Footer` |
| Save result, copy confirmation | `Toast` / `Toaster` / `toast()` |
| Pending reconciliation | `ApplyBanner` |
| Activity log | `LogViewer` + `Toolbar` + `Select` (level) + `ToggleGroup` (stream) + `Input` (search) |
| Diagnostics: check results | `Box` + `Spinner` → `Badge` + `Collapsible` (details) |
| Spool depth, disk usage | `Meter` |
| Long-running progress | `Progress` |
| Empty lists | `EmptyState` |
| Inline help "More" | `Collapsible` |
| Contextual help on a heading | `Tooltip` / `Popover` |
| Keyboard navigation to any page | `Command` |
| Dark / light | `ThemeToggle` |
| Setup code entry | `PinInput` — see gap G7 |
| Page-level tabs (Settings / Status / Diagnostics) | `Tabs` |
| Advanced: collapsed sections | `Accordion` |

**Not used, and why:** `Calendar`, `RangeCalendar`, `DateField`, `DatePicker`, `DateRangeField`,
`DateRangePicker`, `TimeField`, `TimeRangeField` (nothing in a configuration tool takes a date —
historical charting is out of scope), `RatingGroup`, `Avatar`, `AspectRatio`, `LinkPreview`,
`NotificationBadge`, `Menubar`, `ContextMenu` (right-click menus are undiscoverable; `DropdownMenu` on
an explicit button is used instead), `Pagination` (the lists here are short; the log view scrolls),
`Listbox`, `ToggleButton`.

Listing the unused set is not padding: it is the evidence that the design was mapped against the
toolkit rather than written first and reconciled afterwards.

### 6.3 Gaps in `chonky`, as findings

Each is stated with what the design does in the meantime. **The toolkit wins by default** where its
conventions and this document's proposals conflict — it is the thing the owner maintains across
projects.

| # | Gap | Impact here | Proposal |
|---|---|---|---|
| **G1** | **No app-shell / sidebar layout primitive.** The owner's own docs site hand-rolls its sidebar in `+layout.svelte`. | Every chonky consumer rebuilds the same shell. | **Local component now**, built from `Box`/`Separator`/`ScrollArea`/`Drawer`, following the docs site's structure. Worth promoting to chonky as `AppShell` later; not this project's call. |
| **G2** | **`Dot` is boolean** (`on?: boolean`). | Six lifecycle states need six tones. | **Propose `Dot { tone?: 'neutral'\|'success'\|'warning'\|'danger'\|'info', pulse?: boolean }`**, keeping `on` as an alias. A three-line change, useful to every consumer. Meanwhile: `Badge` carries the colour and `Dot` carries only live/not-live. |
| **G3** | **No `Badge` variant for "in progress".** Variants are `default\|success\|warning\|danger\|info`. | `Starting` and `Stopping` have no honest colour; `info` reads as a notice. | Use `info` + `Spinner`. Propose a `pending` variant. |
| **G4** | **Validation is inconsistent across form controls.** Only `Input` has `error?: string`; `Select`, `Toggle`, `Checkbox`, `Combobox`, `Slider` and `PinInput` do not. | Field-level validation (§8.5) cannot be expressed uniformly. | **Local `Field` wrapper** owning label + hint + error + `aria-describedby`/`aria-invalid` for any control. Propose `error?: string` on the remaining controls, or a chonky `Field`. **The most valuable of the gaps** — it is the one that otherwise produces two validation vocabularies. |
| **G5** | **`Label` has no hint/description slot.** | §0.5 requires a hint line on every field. | Covered by G4's `Field`. Propose `Label { hint?: string }`. |
| **G6** | **No clipboard-copy affordance**, though a `copy` icon exists. | The one-time setup code, a peer certificate fingerprint and (if adopted) an API token are all shown-once, copy-exactly values. | **Local `CopyField`**. Propose `CopyField` to chonky; it is generic. |
| **G7** | **`PinInput` is a fixed-length short-code control.** The setup code is 32 base32 characters in 8 groups. | Not a fit as-is. | **A plain `Input` that normalises** (upper-cases, strips dashes and spaces) plus `CopyField` on the console side. Evaluate `PinInput` with `length=32` during implementation; do not force it. |
| **G8** | **`LogViewer` does not virtualise** — it renders every entry. | 4,096 + 16,384 ring records would be ~20k DOM rows. | **Cap the client buffer at 2,000 entries**, newest-wins, with "older entries are available in the full log" linking to the paged `GET /api/v1/logs`. Report virtualisation to the owner as a finding; do not work around it with a second log component. |
| **G9** | **No `@font-face` for Inconsolata.** `tokens.css` names the family; the package ships no font. | A Pi with no internet falls back to Courier New and the whole design language degrades. | **Self-host a woff2 subset** in the UI (§6.5 budgets it). Propose that chonky document the requirement explicitly — it will bite every consumer once. |
| **G10** | **The icon allowlist (38) lacks operational icons.** No `play`, `pause`, `square`, `database`, `server`, `hard-drive`, `cloud-upload`, `alert-triangle`, `info`, `lock`, `key`, `terminal`, `link`, `thermometer`, `activity`. | Run-now, restart, the storage section and several status affordances have no icon. | Extend the allowlist — `icons.ts` documents the three-step process. Fifteen icons, tree-shaken, cost nothing. **Propose upstream rather than importing `lucide-svelte` directly in the UI**, which would silently introduce a second icon vocabulary. |
| **G11** | **No skeleton/placeholder component**; `Spinner` only. | First paint of the station list and the component list. | `Spinner` inside `Box`. Low priority; this UI's payloads are small. |
| **G12** | **No definition/description list.** Key-value detail pairs are everywhere in an admin UI. | Station details, system info, check results. | `Table` with hidden headers reads correctly and is semantically defensible. Propose `DescriptionList`. Low priority. |

**No second component vocabulary is introduced.** Every local component above is a composition of
chonky primitives, lives in `ui/src/lib/components/`, and is listed here so the set is auditable:
`AppShell`, `Field`, `CopyField`, `StatusBadge`, `ScheduleLine`, `CheckResult`, `FirstRunChecklist`.
Seven. Any eighth needs a reason in the PR description.

### 6.4 Build and packaging

**SvelteKit 2 + `@sveltejs/adapter-static` + Vite 6, in SPA mode.** `ssr = false`,
`prerender = false`, `fallback: 'index.html'`.

- **Why SvelteKit rather than bare Vite + Svelte:** file-based routing, automatic per-route code
  splitting, and typed `load` functions, for a few kilobytes of runtime. The decisive factor is that
  it is **what the owner already uses** — chonky's own docs package is SvelteKit 2 + `adapter-static`
  3 + Vite 6. Matching it means one mental model across the owner's projects.
- **Why SPA rather than SSR:** the server is a Rust binary serving an embedded bundle. There is no
  Node runtime in production, and introducing one would be a second deployment artefact for a
  single-operator admin tool.
- **Location:** `ui/` at the repository root, a pnpm project, outside the cargo workspace. `pnpm`
  because that is what chonky uses.
- **Embedding:** `rust-embed` in `mw-api-mgmt`, matching W3 §7.2's decision for the public site's
  assets and the Go version's `embed.FS`. One static binary; `--assets-dir` in debug builds for
  development.
- **Dev loop:** `vite dev` on :5173 proxying `/api` to a running daemon. The daemon never needs to
  rebuild to iterate on the UI.
- **Hashed filenames** under `/assets/*`, `Cache-Control: public, max-age=31536000, immutable`;
  `index.html` is `no-cache`. The current UI has neither.
- **CI:** a `ui` job on the PR workflow — `pnpm install --frozen-lockfile`, `pnpm check`
  (`svelte-check`), `pnpm lint`, `pnpm build`, `pnpm test`, then `cargo xtask gate ui-bundle`. Wrapped
  as an `xtask` gate so there is one gate vocabulary across the repository (W8's pattern), and listed
  in `ci/gates.toml` so its armed/deferred state is printed into every run's summary.
- **Release:** `cargo xtask dist` builds the UI before the Rust binaries, so a release artefact can
  never contain a stale bundle.

### 6.5 The bundle budget

Stated as numbers, with what counts, because a budget without a definition is a wish.

| Measure | Budget |
|---|---|
| **Initial route** — JS + CSS needed to render Home after sign-in, minified + gzipped | **150 KB** |
| **All routes** — every lazily-loaded chunk summed, minified + gzipped | **280 KB** |
| **Self-hosted Inconsolata** — woff2, Latin subset, 2 weights | **45 KB** (separate line) |
| **Largest single lazy chunk** | **60 KB** |

**What counts:** every first-party and third-party `.js` and `.css` the browser must fetch to use the
application, measured gzipped, as emitted by `vite build`. **What does not:** fonts (budgeted
separately above), SVG/PNG assets, source maps (not shipped), and the generated `.d.ts` (types,
compiled away).

**Why these numbers are achievable rather than aspirational.** Svelte compiles the framework away;
chonky is tree-shaken at the component level by Vite and this design uses roughly 30 of its 60
components; `openapi-fetch` is ~2 KB and `openapi-typescript` emits nothing at runtime;
`lucide-svelte` icons are tree-shaken per icon through chonky's barrel (documented in `icons.ts`), so
the allowlist costs only the icons actually rendered. The realistic measured outcome is well under
these numbers; they are set with headroom so the gate is a ratchet against carelessness, not a
constant source of noise.

**Enforcement:** `cargo xtask gate ui-bundle` reads `ui/budget.toml`, compares the gzipped output, and
**fails the build** on an over-budget bundle, printing the top five chunks by size and the delta
against the previous commit. Raising a budget is a reviewed diff to `budget.toml`, which is the point.

**Three explicit prohibitions**, each a defect in the current UI:

1. **No moment.js, and no date library at all** unless a budget diff justifies it. The current UI
   vendors a deprecated copy. `Intl.DateTimeFormat` and `Intl.RelativeTimeFormat` are built in, handle
   time zones properly, and cost zero bytes.
2. **No CDN.** Everything is embedded. A weather station on a LAN with no internet must have a working
   admin UI — and a CDN is also a third party that learns when the operator signs in.
3. **No chart library.** Charting is out of scope (D-38; the public website already does it). If a
   sparkline is ever needed, it is inline SVG.

### 6.6 Enforcing the vocabulary rule

§0.1 says an internal term on screen is a bug. A rule that is not checked is a preference, so:

- All user-facing strings live in `ui/src/lib/copy/` as typed modules (not inline in components), one
  module per section, with the §0.5 hint text alongside each label.
- `cargo xtask gate ui-copy` greps the copy modules and every `.svelte` template for a banned-word
  list — `controller`, `storage backend`, `component`, `capability`, `trigger`, `sink`, `source`,
  `envelope`, `spool`, `provision`, `supervisor`, `engine`, `device`, plus every `Capability` variant
  name — and fails the build on a hit.
- The escape hatch is explicit and greppable: `raw('ReadingStore')` marks a deliberate internal term,
  permitted only in `System → Advanced` modules. The gate prints the current exception list on every
  run, so it cannot grow quietly.
- **`device` is on the banned list and `station` is not.** The current UI alternates between them
  within a single screen; the lint is what stops that recurring.

### 6.7 Testing

Proportionate: this is a configuration tool for one operator, not a consumer product.

- **Vitest** for the pure logic worth testing in isolation: the vocabulary mapping, the
  `applied_generation` "is this live?" derivation, the SSE reconnect/backfill state machine, and the
  §0.4 status-sentence renderer. These are the parts where a bug is silent.
- **`svelte-check` / `tsc --noEmit`** is the API-drift gate (§4.3), and it is the highest-value check
  in the list.
- **Playwright, for five flows only**, run against a real `modernweather` binary with a seeded config
  database: first-run claim, add a station, save-and-see-it-apply (§2.4), sign out everywhere, and the
  activity log reconnecting across a daemon restart. Five, because these are the flows where an
  integration bug is invisible to every other check — and because a large browser-test suite for an
  admin tool is a maintenance cost the owner would carry alone.
- **No snapshot tests of rendered markup.** They fail on every chonky upgrade and catch nothing this
  design cares about.

### 6.8 Rust in the browser — closed

Leptos/Dioxus were considered and are **rejected**, recorded here so the question does not reopen. The
reason is not a technical comparison: an existing, maintained, owner-authored Svelte toolkit reused
across the owner's projects is a stronger argument than any type-sharing benefit. Type sharing is also
already solved by §4 — the OpenAPI schema is generated from the Rust handlers, so the types *are*
shared, through a seam that fails the build on drift. Rust-in-the-browser would buy a worse version of
a property this design already has, at the cost of abandoning `chonky`.

---

## 7. Information architecture

### 7.1 Page inventory

`C` = validated client-side (format, required, range — instant feedback).
`S` = validated server-side (authoritative; `plan()` and the store).
Everything validated `C` is **also** validated `S`; client validation is a convenience and never a
security or correctness boundary.

| Path | Page | Shows | Editable | Validation |
|---|---|---|---|---|
| `/` | **Home** | Root-cause attention panel (§2.5); counts; one tile per station with its latest reading; first-run checklist until complete | nothing | — |
| `/stations` | **My weather stations** | List: name, kind, "Collecting readings" toggle, status badge, last reading age | the toggle (inline) | `S` |
| `/stations/new` | **Add a station** | §0.7 step 2: type chooser, discovery, serial-port picker, Test connection | all fields | `C` type/host/port/baud format, required-by-type; `S` name uniqueness, `plan()` |
| `/stations/{id}` | **Station — Settings** | Name, kind, connection, location, altitude, time zone, wind-direction correction, snow base distance; read-only "Shared with:" line linking to `/share` | all | `C` numeric ranges, IANA time zone, latitude/longitude; `S` uniqueness, `plan()`, route conflicts |
| `/stations/{id}/status` | **Station — Status** | Live reading panel; state + schedule sentence (§0.4); counters; last error with cause chain | nothing | — |
| `/stations/{id}/diagnostics` | **Station — Diagnostics** | Test connection, Read current values; per-station log filter link | nothing | — |
| `/share` | **Share my data** | One row per destination × station — Weather Underground, PWS Weather, APRS/CWOP, Forecasts & alerts — with on/off, status badge, "last sent" | per-row on/off | `S` |
| `/share/weather-underground` | **Weather Underground** | Per-station station ID + password, upload interval, API endpoint; global credentials; status; last upload; last error | all | `C` interval ≥ 60 s (warn, not block — §8.5), required-if-enabled; `S` upstream rejection surfaces as a failed state, not a save error |
| `/share/pws-weather` | **PWS Weather** | Same shape | all | same |
| `/share/aprs` | **APRS / CWOP** | Callsign, passcode, symbol, comment, interval; **Send over:** APRS-IS *or* a TNC radio; TNC: serial device + baud *or* TCP address; digipeater path, destination; Test | all | `C` callsign format, passcode numeric, interval ≥ 300 s; `S` `plan()` (a TNC serial port is exclusive) |
| `/share/forecasts` | **Forecasts & alerts** | On/off, Aeris client ID + secret, refresh interval, coordinates; last forecast and last alert fetch times (`Freshness`, §0.4) | all | `C` required-if-enabled; `S` the credential test is a check, not a save gate |
| `/website` | **My website** | List of sites: name, address, which station feeds it, status, certificate expiry | — | — |
| `/website/{id}` | **Website — Settings** | Name, hostname, page title, "About this station" HTML, which station, snow on/off + which sensor, air quality on/off + which sensor, Apple app ID, portal flag | all | `C` hostname format; `S` hostname uniqueness, `RouteConflict` from `plan()` (W3 §6.2) |
| `/website/{id}/features` | **Website — Extras** | Weather radar map: register / registered-on / re-register / turn off | the radar toggle | `S` registration is a remote call; its failure is shown inline |
| `/website/{id}/certificate` | **Website — Security certificate** | Certificate and key paths, subject, expiry, SNI note; "Expires in 12 days" warning | paths | `C` path non-empty; `S` file readable, parses, key matches certificate, not expired — all as *warnings on save*, never blocks |
| `/data-storage` | **Data storage** | Weather database (status, size, oldest reading); Send to another RemoteWeather; Receive from another RemoteWeather | — | — |
| `/data-storage/database` | **Weather database** | Host, port, database, user, password, SSL mode, time zone; Test connection; status; summary-history state | all | `C` host/port/required; `S` connect test on demand, `plan()` |
| `/data-storage/send` | **Send my readings to another RemoteWeather** | Peer address, TLS on/off, status, **unsent readings** depth (`Meter`), last successful delivery | all | `C` address format; `S` `plan()` |
| `/data-storage/receive` | **Receive readings from another RemoteWeather** | Listening port, certificate, the peer registry (`GET /api/v1/peers/stations`), per-peer last-seen | port, certificate, peer approval | `S` port conflict via `plan()` |
| `/system` | **System** | Section index with a one-line status per subsection | — | — |
| `/system/activity` | **Activity** | `LogViewer` over the merged stream; level / stream / station / text filters; "Show more detail" (§5.5); terminal view; Download | filters only | — |
| `/system/diagnostics` | **Diagnostics** | All seven checks with Run buttons and last results; full component list with state, schedule, counters | run-now / restart per item | — |
| `/system/machine` | **This machine** | Version, uptime, OS, architecture, memory, disk, serial ports present, listening ports | nothing | — |
| `/system/administrator` | **Administrator** | Username; change password; active sign-in sessions with revoke; "Sign out everywhere"; session lifetime | password, lifetime | `C` length ≥ 12, confirmation match; `S` current password, rate limit |
| `/system/advanced` | **Advanced** | Admin port/bind/certificate; CORS allowlist; standing log-level overrides; raw configuration (secrets omitted) with Download; internal component ids; metrics link | admin listener, CORS | `S` `plan()`; a port change shows the new URL and requires explicit confirmation |
| `/setup` | **First-run claim** | §3.4 — the only page reachable while unclaimed | code, username, password | `C` code format, length ≥ 12, match; `S` single-use code, atomic claim |
| `/sign-in` | **Sign in** | Username, password | both | `S` only — a client-side hint about *which* field was wrong is a user-enumeration oracle |

### 7.2 Validation philosophy

- **Client-side validation is for speed, never for safety.** Every `C` rule has an `S` counterpart.
  The UI never disables Save on the basis of a client rule alone, except where the field is
  structurally unusable (an empty required field).
- **Server-side validation returns field paths**, so errors land on the field rather than in a banner:
  `{"field": "stations[0].hostname", "message": "Enter a hostname or IP address."}` — §8.5.
- **`plan()` errors are validation errors.** `PlanError::RouteConflict { scope, pattern, a, b }`
  (W3 §6.2) is rendered on the field that caused it: "Another website already uses
  `weather.example.com`." That is only possible because `plan()` is pure and runs before anything
  stops (W2a §2.5) — the `POST /api/v1/config/preview` call behind the Save button is the same call.
- **A connectivity failure is not a validation failure.** Saving a database password the database
  rejects is a legitimate save — the database may be down, or about to be reconfigured. The save
  succeeds, the component moves to `Failed`, and §2.4's banner ⑤ reports it. Conflating "I can't
  reach it" with "that input is invalid" is how a UI ends up refusing to let an operator fix a
  configuration while the thing it configures is down.

### 7.3 Where the current structure actively misleads

Called out as the prompt requires, each with its replacement.

| Current | Why it misleads | Replacement |
|---|---|---|
| Tabs named **Controllers** and **Storage** | Names the implementation. A new user cannot map "I want my data on Weather Underground" onto either. | §0.6's intent-based navigation |
| **Save** on a controller's settings | Reports success for a change that does not take effect until restart (`internal/managers/controller.go:177-190`) | §2.4's preview → apply → live sequence |
| **Delete** a controller | Reports success; the goroutines keep running (`internal/managers/controller.go:148-152`) | §2.4, plus a real stop (W2a §3.5) |
| Saving **storage** settings | Silently tears down the gRPC-stream uplink, which no screen mentions (`internal/managers/storage.go:158-166`) | The preview names every affected item before the save |
| **Storage health** on its own page | A second, 90-second-ticker health mechanism parallel to component state | One status surface (§2.3); W2a §3.2 deletes the parallel path |
| The **weather-station form** | Shows all 50+ `DeviceData` fields regardless of station type — APRS KISS TNC settings on an AirGradient sensor | Type first, then only that type's fields (§0.7) |
| **`/logs`** and **`/http-logs`** as separate pages | Correlating a 500 with the query that failed means two screens and two clocks | One merged view with a stream filter (§1.8, W3 §5.7) |
| **Utilities → Change token** | The thing it rotates is simultaneously the API credential and the session cookie, so rotating it signs you out of a session you cannot otherwise revoke | `System → Administrator` (§3.3) |
| **No indication of what is running** | The deepest one: configuration is presented as the system's state | §2 in its entirety |

### 7.4 Accessibility and responsive behaviour

Not a separate work item; specified here so it is not retrofitted.

- Keyboard reachable throughout — `bits-ui` provides focus management and ARIA for every overlay and
  menu. The local components (§6.3) must match: `Field` wires `aria-describedby` to the hint and
  `aria-invalid` + `aria-describedby` to the error.
- **Colour is never the only signal.** Every status badge carries a word (§0.4's Badge column), not
  just a colour. This matters for the status vocabulary and for the ERROR/WARN/INFO colouring in the
  log view.
- `prefers-reduced-motion` respected: the `Backoff` countdown and the live `Dot` stop animating.
- **Responsive down to a phone**, because checking a station from the garden is the real case:
  `Drawer` for navigation, `LogViewer`'s built-in `card` mobile layout, tables degrading to stacked
  rows, and a minimum 44px touch target on every control. §0.5's "always-visible hint rather than
  hover" is partly an accessibility decision and partly this one.
- Contrast comes from chonky's theme tokens rather than hard-coded colours, in both themes — which is
  also why W3 §5.8's log palette is applied as theme tokens rather than as ANSI colours (W3
  assumption 36).

---

## 8. Error and empty states

Most of an admin UI's real screen time, and the usual place design effort does not go. Each of the
prompt's six is specified, plus two the inventory turned up.

### 8.1 No stations configured yet (first-run)

**Not an empty state — the primary onboarding flow.** §0.7. The operator never sees a bare "No devices
configured" line; they see the welcome checklist with step 2 highlighted. If they navigate to
`/stations` directly:

```
┌──────────────────────────────────────────────────────────────────────┐
│                     No weather stations yet                          │
│                                                                      │
│   RemoteWeather collects readings from a weather station and         │
│   keeps them for you. Let's connect your first one.                  │
│                                                                      │
│                    [ Connect a weather station ]                     │
│                                                                      │
│   Not sure what you have? We'll help you work it out.                │
└──────────────────────────────────────────────────────────────────────┘
```

`EmptyState`. One action, no secondary clutter. The same pattern — explain, then one action — applies
to the empty website list, the empty share list and the empty peer list.

### 8.2 A station configured but failing to connect

```
┌─ Backyard ────────────────────────────── ✗ Stopped ─────────────────┐
│                                                                      │
│  Can't reach your Davis station.                                     │
│  Tried: serial port /dev/ttyUSB0 at 19200 baud · last tried 8s ago   │
│  Retrying every few seconds.                                         │
│                                                                      │
│  Most likely:                                                        │
│    · the console isn't plugged in, or the USB cable is power-only    │
│    · another program is holding the port                             │
│    · the baud rate doesn't match the console's setting               │
│                                                                      │
│  [ Test connection ]  [ Change settings ]  [ Details ▾ ]             │
└──────────────────────────────────────────────────────────────────────┘
```

- **Names what was tried**, in the operator's terms, including the actual port and speed.
- **"Most likely" is per transport**, not generic: serial gets the three causes above; TCP gets
  "wrong address", "firewall", "console is asleep"; an inbound station ("Ambient Weather", "Receive
  from another RemoteWeather") gets "nothing has sent us a reading yet" plus the URL the device should
  be pointed at, because for those the failure is on the *other* end.
- **`Details`** holds the full cause chain. W3 §4.3 preserves it and the JSON layer emits it as an
  array (W3 assumption 33 names this as what W5 renders as an expandable chain):
  `open /dev/ttyUSB0 → Permission denied (os error 13)`.
- A permission error gets a specific, actionable line — "RemoteWeather doesn't have permission to use
  this port. On most systems: `sudo usermod -aG dialout modernweather`, then restart." — because it is
  the single most common first-install failure on Linux.

### 8.3 Storage unreachable

Covered structurally by §2.5: **one root cause with its blast radius**, on Home and on the
`/data-storage/database` page, and *not* as five independent red badges.

On the database page itself:

```
┌─ Weather database ─────────────────────── ⟳ Retrying ───────────────┐
│  Can't connect to the weather database. Retrying in 12 seconds.     │
│  Last connected: 21:12:40 (4 minutes ago)                           │
│                                                                      │
│  While this is down:                                                 │
│    · new readings are being held and will be saved when it returns   │
│    · your website and the phone app are still working                │
│    · uploads to weather services are paused                          │
│                                                                      │
│  [ Test connection ]  [ Change settings ]  [ Details ▾ ]             │
└──────────────────────────────────────────────────────────────────────┘
```

Every line is a fact the new architecture makes true and the old one did not: readings are held
because W7's durable spool backs `writer:timescale`; the website and app are unaffected because
`/latest` is served from `snapshot:latest` (W3 §2a.6); uploads are paused rather than dead because the
uploaders sit in `Waiting` and resume (W2a §3.4, D-27). **The screen's reassurance is only honest
because the architecture changed** — which is the point of §2.

### 8.4 A sink backed up with a deep spool

"Spool" never appears. The operator sees **unsent readings**:

```
┌─ Send to another RemoteWeather — weatherbox.example.com ── ⟳ Retrying ─┐
│  Can't reach the other RemoteWeather. Retrying in 30 seconds.          │
│                                                                        │
│  1,284 readings waiting to be sent       [████████░░░░░░░░] 46 MB      │
│  Oldest: 2 hours 14 minutes ago                                        │
│  Nothing has been lost — they'll be sent when the connection returns.  │
│                                                                        │
│  [ Test connection ]  [ Details ▾ ]                                    │
└────────────────────────────────────────────────────────────────────────┘
```

- `Meter` with `low`/`high` thresholds against the spool's configured capacity, so "getting full"
  reads before "full" happens.
- **The reassurance is conditional on the sink's delivery policy**, and the UI reads the policy rather
  than assuming it. D-30's asymmetry is deliberate: durable sinks must not lose data, rate-limited
  upload sinks coalesce to the newest reading. So:
  - a **durable** sink (the database, a peer uplink) says "Nothing has been lost";
  - a **rate-limited upload** sink (Weather Underground, PWS, APRS) shows no spool at all and instead
    says "**Only the most recent reading is sent.** Readings from while this was down are not
    uploaded — these services want current conditions, not history."

  Saying "nothing is lost" about a coalescing sink would be a lie, and it is exactly the lie a generic
  "backed-up queue" component would tell.
- **Approaching capacity** escalates: at the high threshold the row turns warning and reads "If this
  keeps growing, the oldest unsent readings will be dropped to make room."

### 8.5 A validation failure on save

**Errors land on fields, never only in a banner.**

```
  Upload interval
  [ 30                                    ]  seconds
  ⚠ Weather Underground asks for no more than one upload a minute.
    Using 30 seconds may get your uploads rejected.        ← warning, saves fine

  Station ID
  [                                       ]
  ✗ Enter the station ID Weather Underground gave you.      ← error, blocks save
```

- **Warnings and errors are different.** A warning explains a consequence and does not block (the
  operator may know something the validator does not); an error blocks and says what to type. Today's
  UI blocks on both and explains neither.
- **On submit with errors:** focus moves to the first invalid field, the page scrolls to it, and the
  summary at the top of the form names each one as a link. Not a toast — a toast is dismissible and
  the operator then cannot find the field.
- **Server-side errors use the same slots.** The `422` body's `field` paths map to the same inline
  positions, so an error the client did not predict looks identical to one it did.
- **Errors never discard input.** The form keeps what was typed, including the invalid value. This is
  listed because the current UI's save path reloads the form from the server on error.
- **Secret fields are the exception to "keep what was typed":** a password field that failed
  validation keeps its value for the session but is never re-rendered from a server response, because
  no server response contains it (§3.2).

### 8.6 A config change accepted but not yet in effect

§2.4, states ② through ⑤, verbatim. This is the state the whole of §2 exists to make visible, and it
is the one state the current UI cannot represent at all.

The escalation ladder, restated as timing: **immediate** (saved, catching up) → **10 seconds** (saved,
not running yet, with the reason) → **on failure** (saved, won't start, with Details and Undo). Never
a spinner that runs forever, and never a success message that outlives the truth.

### 8.7 A deletion that breaks something else — added

Not in the prompt's list, but the inventory produced it: a station can feed a website
(`WeatherWebsiteData.DeviceID`), can be a website's snow or air-quality source, and can be the subject
of PWS/WU/APRS uploads. Deleting it today leaves dangling references.

```
┌─ Delete "Backyard"? ────────────────────────────────────────────────┐
│  This will also:                                                     │
│    · leave your website "home.example.com" with no weather to show   │
│    · stop the Weather Underground and APRS uploads for this station  │
│                                                                      │
│  Readings already saved in the weather database are kept.            │
│                                                                      │
│                              [ Cancel ]  [ Delete this station ]     │
└──────────────────────────────────────────────────────────────────────┘
```

`AlertDialog`. The consequence list is computed server-side by the same `POST /api/v1/config/preview`
call as §2.4 — the deletion's plan diff — so it cannot drift from what actually happens. The last line
matters: an operator deleting a misconfigured station needs to know their history survives.

### 8.8 Loading, offline, and signed-out — added

Three states that are not failures and must not look like them:

- **Loading:** `Spinner` inside the destination `Box`, never a full-page blocker. The shell and
  navigation render immediately.
- **Stream disconnected:** §5.2 — the `StatusBar` indicator, panels dimmed with "last updated {time}".
  The page stays usable; forms still save over ordinary HTTP.
- **Session expired:** a sign-in `Modal` **over the current page**, not a redirect that discards
  context. On success the page resumes exactly where it was and any in-flight save is retried once. A
  redirect here would throw away a half-filled station form, which is the moment an operator is least
  forgiving.

---

# Decisions

1. No internal term from W2a, W3 or the current Go implementation appears in any label, status
   message, error, URL or heading; the ban is enforced by a CI lint over the copy modules, not by
   review (§0.1, §6.6).
2. The internal→on-screen translation table in §0.2 is authoritative, and it is implemented **once,
   on the server**, as `display.name` / `display.qualifier` / `display.section` on every runtime
   status object — not in the client (§2.3).
3. Three terms stay technical because their audience already uses them: APRS/CWOP vocabulary, serial
   port and baud rate, and hostname/port/TLS certificate (§0.3).
4. Every one of W2a's nine lifecycle states, six triggers and ten counters has a specified
   plain-language rendering (§0.4).
5. Freshness and staleness are rendered **separately from component state**, because
   available-but-stale is not a failure (§0.4; W3 assumption 39).
6. Every field has an always-visible one-line hint (≤ 90 characters) plus an optional "More"
   disclosure. Hover-only help is not used anywhere (§0.5).
7. Navigation has six top-level items organised by operator intent: Home, My weather stations, Share
   my data, My website, Data storage, System (§0.6).
8. The management API's own configuration gets no page of its own; its port, bind address and
   certificate move to `System → Advanced` (§0.6, §1.4).
9. First-run is a **resumable checklist, not a modal wizard**, and it subsumes the empty-state design
   (§0.7, §8.1).
10. On the "add a station" screen, WeatherLink discovery runs automatically before the type is
    chosen, and serial ports are always a picker, never typed (§0.7).
11. Every station form's primary action is **Test connection**; Save stays enabled regardless, so a
    not-yet-plugged-in station can still be configured (§0.7).
12. The measured management route count is **47 API registrations** (plus 9 SPA pages and 3 asset
    prefixes), not 46; all 59 are mapped in §1, along with 2 endpoints the shipped client calls that
    the server does not register (§1.13).
13. The `/config/controllers/{type}` family is dissolved; its contents are redistributed to
    per-service pages and `System → Advanced`, and no screen lists "controllers" (§1.4).
14. All six `/test/*` checks survive, plus a seventh — the APRS connectivity check W2a §3.2 extracts
    from the 90-second health monitor — and all seven share one request path, one response shape and
    one results component (§1.7).
15. `POST /api/config/reload` is **dropped**: a Reload button re-teaches the operator that saving
    does not take effect, which is the defect being removed (§1.10).
16. Storage health merges into component state; the parallel `GlobalHealthManager` surface is not
    carried forward (§1.3).
17. A **monotonic configuration generation** is assigned on every write and echoed as
    `applied_generation` on every component's runtime status; a change is "live" exactly when every
    affected component's `applied_generation ≥ generation` (§2.2).
18. **Saving applies.** There is no staging area and no Apply button; `ApplyBanner` reports
    reconciliation progress, not a queue (§2.4).
19. Every save is preceded by a `POST /api/v1/config/preview` dry run of the supervisor's pure
    `plan()`, so the operator sees exactly what will restart **before** agreeing (§2.4).
20. A save that has not taken effect after 10 seconds escalates to a named explanation, and a save
    whose component then fails offers **Details** and a one-step **Undo save** (§2.4).
21. The server computes the dependency rollup and returns root causes with their blast radius, so one
    unhealthy database reads as one problem blocking five things, not five outages (§2.5).
22. The UI offers **run-now** and **restart**, but **no general start/stop**: turning something off is
    a configuration change, so runtime state never diverges from configured state (§2.6).
23. Passwords are stored as full Argon2id PHC strings at m=64 MiB, t=3, p=4 — chosen for
    Raspberry-Pi-class hardware — and re-hashed transparently on login when below policy (§3.2).
24. A session is a fresh 32-byte random secret stored as a SHA-256 hash, delivered as an `HttpOnly`,
    `SameSite=Strict`, `Secure`-when-TLS cookie, individually revocable, with "sign out everywhere";
    the cookie is never the API credential (§3.3).
25. Changing the password revokes every other session and keeps the current one (§3.3).
26. CSRF defence is `SameSite=Strict` **plus** a required `X-MW-Request` header on every
    state-changing request (§3.3).
27. First start with no administrator generates a single-use setup code, prints it once to **stderr
    only and never to the log ring**, and requires it to create the account; the code is invalidated
    in the same transaction that creates the account (§3.4, §3.5).
28. The "first to reach the setup page owns the install" hazard is recorded in the design with its
    reasoning, so that removing the setup code has to argue with it (§3.4).
29. The management API binds **loopback while unclaimed**, with an SSH-tunnel command printed in the
    console banner and an explicit `--setup-listen` opt-out for headless remote installs (§3.4).
30. The management API's default `listen_addr` changes from `0.0.0.0` to **`127.0.0.1`** (§3.5).
31. `Access-Control-Allow-Origin: *` is replaced by an exact-match allowlist defaulting to empty
    (same-origin only) (§3.5).
32. Failed logins get a per-username progressive delay (`min(2^(n-3), 30)` s, applied before
    verification), never a lockout (§3.5).
33. Minimum password length is 12 with **no composition rules**, per current NIST guidance (§3.5).
34. Recovery is `modernweather reset-password` on the host; the trust boundary is local filesystem
    access, which already protects every other secret (§3.6).
35. An optional `MW_INITIAL_ADMIN_PASSWORD` / `_FILE` is offered for provisioning, with its exposure
    stated plainly; the `_FILE` form is preferred (§3.4).
36. **No API tokens in v1.** Nothing needs one; the shape for adding one later is fixed in advance so
    it cannot regress into "the cookie is the token" (§3.7).
37. The management OpenAPI schema is **derived from the Rust handlers** via `utoipa` and emitted by
    `cargo xtask codegen` into `generated/openapi/management.json`; it `$ref`s W2's reading schema
    rather than redefining it (§4.1).
38. The TypeScript client is `openapi-typescript` (types only) + `openapi-fetch` (~2 KB), with the
    `.d.ts` committed and drift-gated — not a class-generating client (§4.2).
39. Drift fails the build by two independent mechanisms: `cargo xtask gate codegen-drift` for a stale
    schema, and `svelte-check` / `tsc --noEmit` for a stale UI (§4.3).
40. Live data is **one SSE endpoint with multiple channels**, giving one reconnect path, one backfill
    path and one gap story (§5.1).
41. Reconnect honours `Last-Event-ID`, backs off 1→15 s with jitter, and always shows the operator
    what the connection is doing; no panel presents stale data as live (§5.2).
42. Backfill is the ring's current contents followed by a `caught_up` marker, so "— live —" is drawn
    in the right place (§5.3).
43. Ring overflow renders W3's synthetic `gap` record as an inline "N entries dropped" marker, with an
    explanation after repeated gaps (§5.4).
44. View filtering (client-side, non-destructive) and capture level (`set_level(target, level, ttl)`)
    are presented as two distinct controls; the target picker uses plain names (§5.5).
45. A log-level raise defaults to a 10-minute TTL; a TTL-less raise is allowed and shown as a standing
    condition until cancelled (§5.5).
46. No page polls. The only client-side timer is the `Backoff` countdown rendering a server-supplied
    absolute timestamp (§5.6).
47. Every screen is mapped to `chonky` components (§6.2), and the twelve gaps found are reported as
    findings with proposals rather than worked around (§6.3).
48. Exactly **seven** local components are introduced, each a composition of chonky primitives, and
    they are enumerated so an eighth needs justification (§6.3).
49. The build is **SvelteKit 2 + `adapter-static` + Vite 6 in SPA mode**, matching the stack chonky's
    own docs site uses; the bundle is embedded with `rust-embed`, matching W3 §7.2 (§6.4).
50. Bundle budget: **150 KB gzipped initial route, 280 KB all routes, 60 KB largest lazy chunk**, with
    a separate 45 KB font line, enforced by `cargo xtask gate ui-bundle` against `ui/budget.toml`
    (§6.5).
51. No date library, no CDN, no chart library — each a named defect of the current UI (§6.5).
52. Testing is Vitest for pure logic, `svelte-check` as the drift gate, and **five** Playwright flows
    against a real binary; no markup snapshot tests (§6.7).
53. Rust-in-the-browser is **closed**: considered and rejected in favour of the owner's maintained
    Svelte toolkit, and the type-sharing argument is already satisfied by §4 (§6.8).
54. Client-side validation is for speed and never for safety; every client rule has a server
    counterpart, and server errors return field paths that land in the same inline slots (§7.2).
55. A connectivity failure is never a validation failure — a save whose target is unreachable succeeds
    and the component reports `Failed` (§7.2).
56. Warnings and errors are visually and behaviourally distinct: warnings explain a consequence and do
    not block; errors block and say what to type (§8.5).
57. A validation failure never discards typed input (§8.5).
58. Empty states explain and offer exactly one action (§8.1).
59. Failure states name what was tried and offer transport-specific likely causes, with the full cause
    chain behind a disclosure (§8.2).
60. Spool depth is rendered as "N readings waiting to be sent", and the reassurance text is read from
    the sink's delivery policy — durable sinks say "nothing has been lost", coalescing upload sinks
    say the opposite, because D-30's asymmetry makes a single generic message a lie (§8.4).
61. Deletion confirmations list downstream consequences computed by the same `plan()` preview as a
    save, so they cannot drift from what actually happens (§8.7).
62. Session expiry shows a sign-in modal over the current page rather than redirecting, so
    in-progress form input is not discarded (§8.8).
63. Status is never conveyed by colour alone; every badge carries a word (§7.4).
64. The UI is responsive down to phone width, because checking a station from the garden is the real
    case (§7.4).

---

# Open questions

Each stated as a decision for the owner, with options and a recommendation.

**OQ-W5-1 — Does the management API bind to loopback until the instance is claimed?**
*Options:* (a) loopback until claimed, widening after (Decision 29); (b) always bind the configured
address, relying on the setup code alone; (c) loopback until claimed **and** after, requiring an
explicit opt-in for LAN access.
*Trade-off:* (a) makes the takeover window empty rather than merely gated, at the cost of a headless
remote install needing an SSH tunnel or `--setup-listen`. (c) is safest and is probably too much
friction for a tool whose normal use is from a laptop on the same LAN.
**Recommendation: (a).** The setup code already closes the hazard; loopback-until-claimed closes it
twice, and the escape hatches are one command each and printed in the banner.

**OQ-W5-2 — Check new passwords against a breached-password list?**
*Options:* (a) no check; (b) ship a small (~10k entry) common-password list and reject exact matches;
(c) query HaveIBeenPwned's k-anonymity API.
*Trade-off:* (c) is out — it is an outbound network call from a tool that must work offline, and it
leaks a password prefix. (b) costs roughly 80 KB in the binary and catches the obvious mistakes. (a)
is defensible for a single-operator LAN tool.
**Recommendation: (b)**, as a non-blocking warning rather than a rejection. It is cheap, it works
offline, and warning-not-blocking respects an operator who knows what they are doing.

**OQ-W5-3 — Any API token at all in v1?**
*Options:* (a) none (Decision 36); (b) exactly one explicitly-created, shown-once, revocable token.
*Trade-off:* nothing requires one today — the iOS app uses the unauthenticated public API. (b) costs
one table and three endpoints and is a second credential path to keep correct forever.
**Recommendation: (a).** Add it when something actually needs it; §3.7 fixes the shape in advance so
adding it later is cheap and cannot regress.

**OQ-W5-4 — Does the schema keep per-station upload settings on the station row?**
Today `DeviceData` carries both the station's hardware configuration and its PWS/WU/APRS/Aeris
settings, while the same services' global credentials live in `ControllerData` — the same service
configured in two places (§1.2).
*Options:* (a) keep the schema and let the UI present one page per service that edits both halves
(what this design does); (b) normalise into a `station_service` table in W4.
*Trade-off:* (a) ships without a migration and hides the seam, but the seam stays and the next person
to touch the configuration layer meets it. (b) is correct, and is W4's ground rather than W5's.
**Recommendation: (b), deferred to W4**, with (a) as the UI's behaviour either way — the page design
does not change.

**OQ-W5-5 — How much configuration history?**
This design keeps **one step** (`POST /api/v1/config/revert`, within the signed-in session) because
that is what the §2.4 failure banner needs.
*Options:* (a) one step (as designed); (b) the last N generations with a "restore this version" view;
(c) none.
*Trade-off:* (b) is genuinely useful for a system whose configuration is edited rarely and
consequentially, and the generation counter already exists — but it needs a retention policy, a diff
view and a restore path, which is a page of its own.
**Recommendation: (a) now, (b) as a named follow-up.** It is additive and nothing in this design
blocks it.

**OQ-W5-6 — Which `chonky` gaps does the owner want upstreamed?**
§6.3 lists twelve. **G4** (consistent `error` on form controls, or a `Field` wrapper) and **G2**
(`Dot` tones) are the two every consumer will hit; **G9** (document the Inconsolata requirement) costs
a README paragraph and prevents a silent degradation; **G10** (fifteen more icons) is a three-step
change `icons.ts` already documents.
**Recommendation: upstream G2, G4, G9 and G10; keep the rest local for now.** `AppShell` (G1) and
`CopyField` (G6) are good candidates once this project has proved their shape in use.

**OQ-W5-7 — Does the management UI get its own hostname, or stay on its own port?**
It currently runs its own `http.Server` on its own port. W3 §6.1 gives one router over N sockets.
*Options:* (a) keep the dedicated port (status quo); (b) serve the UI on the main listener under a
path prefix; (c) serve it on a dedicated hostname via the existing host routing.
*Trade-off:* (b) would put the admin UI on the public port, which is a meaningful exposure change for
a service designed to face the internet. (c) is tidy but needs DNS the operator may not control.
**Recommendation: (a).** A separate port plus a loopback default (Decision 30) is the right posture
for an admin surface, and W3 §6.1 already establishes that "one router" does not mean "one socket".

---

# Assumptions about other work items

The drift detector. Every assumption this document makes about a document it did not write.

**About W2a (`docs/component-model.md`)**

1. `Waiting { on: Vec<(Capability, ComponentId)> }` carries the **provider's id**, and that id can be
   resolved to a user-facing name — §0.4 and §2.5 both depend on it (W2a §3.1).
2. All nine lifecycle states are observable through `ComponentControl`, including the data each
   carries (`error` with its cause chain, `attempt`, `until`, `degraded`, `at`).
3. `ComponentControl` exposes **read state, start/stop, run-now** (W2a Decision 19) and — per W3 §5.5
   — the `tracing` reload handle. This design uses state, run-now, restart and `set_level`; it does
   **not** use general start/stop (Decision 22).
4. **Extension requested:** `ComponentControl` also exposes `applied_generation` per component and the
   newest committed `generation`. §2.2 is unimplementable without it. This is a small widening of a
   capability W3 §5.5 has already widened once, and it is the only new thing this document asks of
   W2a.
5. The per-component counters `skipped_total`, `deadline_exceeded_total`, `failed_total`,
   `restart_total` and `runs_total` are readable **per component**, not only as process-wide
   Prometheus series — the UI renders them per row (W2a Decision 20).
6. `last_run`, `last_outcome` and `next_run` are **supervisor state** for `Every` and `Calendar`
   components, not derived by the UI (W2a §1.3).
7. `Freshness { capability, populated_by, expected_max_age }` is readable per component and is
   distinct from lifecycle state (W2a Decision 20).
8. `plan()` is **pure** and can be run against a *proposed* configuration without side effects.
   §2.4's preview and §8.7's deletion consequences both depend on this (W2a §2.5).
9. A plan diff reports **which components will start, stop and restart**, by id, before anything is
   applied (W2a §2.5).
10. `PlanError` variants carry enough structure to map onto a form field — `RouteConflict { scope,
    pattern, a, b }` is the worked case (W3 §6.2).
11. An unchanged component is genuinely untouched by a reconfiguration, so "saving a website does not
    disturb the APRS schedule" is observable through an unchanged `applied_generation` (W2a §2.5).
12. The `/test/*` checks are `check:<name>` components with `Trigger::OnDemand`, invoked through
    `ComponentControl`, and the APRS connectivity check is among them (W2a §3.2, §4.4).
13. `Staleness::Degraded { missing, since }` is readable for `snapshot:latest` (W3 §2a.6).
14. Sink delivery policy — durable versus coalescing (D-30) — is **readable per sink**, so §8.4 can
    choose the honest reassurance text rather than hard-coding one.

**About W3 (`docs/service-architecture.md`)**

15. The management API is a `RouteTable` with `Scope::Management` provided by `api:management`, and
    `HttpRoutes` late binding means the UI stays reachable when other components are not (W3 §6.2,
    W2a Decision 19). **The management UI being reachable during an outage is load-bearing for this
    entire design.**
16. Log records reach the UI as **structure**, never as formatted text, with `level`, `target`,
    `module`, `spans`, `fields`, `message`, `causes` and `stream` (W3 §5.8).
17. **One** log stream carries both rings with a `stream: app | access` field (W3 §5.7, assumption
    35).
18. The synthetic `{"kind":"gap","stream":...,"dropped":N}` record appears in the merged stream on
    overflow (W3 §5.7).
19. Backfill on connect is the ring's current contents followed by live records, through one code
    path (W3 §5.7).
20. **Divergence, flagged.** W3 §5.8 names the terminal rendering as `GET /api/logs?format=ansi`.
    This document versions the management API, so it becomes `GET /api/v1/logs?format=ansi`. Same
    behaviour, same query parameter, different prefix. W6 should pick one; this document's reason for
    versioning is the W5 prompt's "additive, versioned routes" instruction in its §4.
21. **Divergence, flagged.** W3 assumption 40 lists `/metrics`, `/api/logs` and `/config.json` as part
    of the management API's generated schema. `/config.json` is the **public website's** configuration
    payload (W3 §7.4), served by `mw-web`, so it is not part of the management schema or the generated
    management client. `/metrics` is Prometheus text exposition (W3 §5.10), which is not JSON and is
    not in the OpenAPI schema either — the UI reads the JSON mirror W3 §5.10 provides on the
    management API.
22. `set_level(target, level, ttl)` is callable with a `ttl` and the expiry is enforced by the
    supervisor, not by the client (W3 §5.5).
23. A TTL-less raise is permitted and is readable as a standing override (W3 §5.5).
24. Error cause chains survive to the JSON layer as an array, which is what §8.2's Details disclosure
    renders (W3 §4.3, assumption 33).
25. `disposition` is a closed vocabulary — `stored`, `spooled`, `dropped`, `retried`, `suppressed`,
    `rejected` — and the UI colours and counts it (W3 §5.9).
26. Events on `target = "mw::audit"` are never suppressed by the dedupe layer, which is what makes
    §3.5's authentication audit trail trustworthy (W3 §5.6).
27. `rust-embed` is the asset-embedding crate and is available to `mw-api-mgmt` as well as `mw-web`
    (W3 §7.2, A-W3-1).
28. `mw-api-mgmt` uses `ConfigAdmin` and `ComponentControl` and does **not** depend on `mw-store` or
    `mw-pipeline` directly (W3 §1.2). The endpoints in §2.6 and §4.3 fit within those two
    capabilities; **`applied_generation` (assumption 4) is the one thing that must come through
    `ComponentControl` rather than being computed inside `mw-api-mgmt`.**
29. Adding `utoipa` to `mw-api-mgmt` is an addition to D-32's crate table in the same spirit as
    A-W3-1's six, not a substitution.
30. The management API keeps its own socket and its own port (W3 §6.1 binds N sockets from
    configuration); OQ-W5-7 is the owner's to close.
31. `admit_rejected_total{reason}` is attributable to a station, so §0.4 can render it on that
    station's page (W3 §2.6).
32. `spool_depth_bytes` and `spool_lag_envelopes` are readable per sink, and a spool's **configured
    capacity** is readable too — §8.4's `Meter` needs a maximum, not just a current value.

**About W2 (`docs/metric-registry.md`)**

33. `generated/ts/metrics.ts` is the UI's source for metric labels and units; the UI maintains no
    parallel list (W2 assumption 18).
34. §0's vocabulary table overrides a generated `label` where the two disagree, via an override map in
    the copy module — never by editing the generated file (W2 Decision 32, R-2).
35. `generated/openapi/reading.json` is `$ref`-able from a second OpenAPI document, so the management
    schema does not redefine `Reading` (§4.1).
36. `cargo xtask codegen` can gain targets (`openapi/management.json`, `ts/mgmt-api.d.ts`) without
    changing its contract, and `--check` covers them under the existing `codegen-drift` gate.

**About W1 (`docs/legacy-api-contract.md`)**

37. The frozen contract is the **public** surface only; the management API is not part of it, so every
    route in §1's "where it goes" column and all of §2.6 and §4.3 is additive and unconstrained
    (D-37).
38. The iOS app does not use the management API, which is what makes §3.7's "no API tokens in v1"
    recommendation safe.

**About W4 (database schema)**

39. The `admin_credential` and `admin_session` tables (§3.2, §3.3) are **SQLite** config-database
    tables created by a migration, not by DDL at startup — the "migrations only" rule applied to the
    configuration store.
40. OQ-W5-4 (normalising per-station service settings out of the device row) is W4's call; the UI's
    page design is unaffected either way.

**About W7 (collector)**

41. The peer / remote-station registry is readable through `StationRegistry` for
    `GET /api/v1/peers/stations` (§1.13), and per-peer registration state is visible.
42. A peer uplink's spool is a **durable** sink for §8.4's purposes — "nothing has been lost" is the
    correct reassurance for it.

**About W8 (CI and release)**

43. A `ui` job can join `.github/workflows/pr.yml`, and `cargo xtask gate ui-bundle` /
    `gate ui-copy` fit the existing gate vocabulary and `ci/gates.toml` reporting.
44. `cargo xtask dist` can build the UI before the Rust binaries, so a release artefact cannot embed a
    stale bundle.
45. pnpm and a Node toolchain are acceptable **build-time** dependencies. They are not runtime
    dependencies — the shipped artefact is still one static binary — but they do change what a release
    machine needs installed, which is W8's to confirm.

---

# Proposed amendments to `DESIGN.md`

Per the amendment protocol: decisions are not edited in place, and this document carries on under the
existing decisions.

### A-W5-1 — D-35 ("one administrator") should say what it means about sessions

**Decision affected:** D-35, §7 "Secrets, errors, authentication".

**What was found.** D-35 says "one administrator, no roles, no multiple accounts", which settles
*identities*. It is silent on *sessions*, and the two readings diverge materially:

- **"One identity, many revocable sessions"** — a laptop and a phone can both be signed in, each
  revocable separately, with "sign out everywhere" available. This is what §3.3 designs.
- **"One session at a time"** — signing in anywhere signs out everywhere else.

The second reading would make the session table pointless and would remove the revocation feature
D-35's own rationale asks for ("a browser session cannot be revoked without rotating the credential
every integration uses"). But it is a reading the words permit, and a later reader economising on the
session table would not be obviously wrong.

**What replaces it.** Append to D-35:

> "One administrator" constrains *identities*, not *sessions*. The single administrator may hold
> several concurrent sessions — a laptop and a phone — each independently revocable, with a "sign out
> everywhere" action. This is required by the decision's own rationale: a session that cannot be
> revoked independently of the credential is the defect being removed.

**Status:** proposed, not applied. §3.3 is designed under the first reading and says so.

### A-W5-2 — D-36 should name the enforcement mechanism

**Decision affected:** D-36, §8 "Vocabulary and external surface".

**What was found.** D-36 says an internal term on screen "is treated as a bug, not a documentation
gap", which is the right standard with no mechanism behind it. Every team that has written that
sentence has also shipped the term. §6.6 specifies a CI lint over a copy module with an explicit,
printed exception list — about thirty lines of `xtask` — and without something like it the decision
degrades into a code-review preference within a release or two.

**What replaces it.** Append to D-36:

> Enforcement is mechanical: user-facing strings live in a single copy module, and a CI gate fails the
> build on a banned internal term. Deliberate exceptions are marked explicitly and the gate prints the
> current exception list on every run. **W5 §6.6 owns the mechanism.**

**Status:** proposed, not applied.

### A-W5-3 — D-33's hardening should name the management API's transport defaults

**Decision affected:** D-33, §7.

**What was found.** D-33 covers secret *handling* — a redacting wrapper type and constant-time
comparison — and its rationale cites the current token's logging and cookie defects. It does not
mention two adjacent defects on the same surface that are equally measured and equally one-line
fixes: `Access-Control-Allow-Origin: *` on the admin API
(`internal/controllers/management/controller.go:306`) and a default bind of `0.0.0.0`. §3.5 changes
both. They are *deployment defaults* rather than secret handling, which is presumably why they are
absent — but they belong with the decision a reader consults when asking "what did we decide about
admin security?"

**What replaces it.** Append to D-33:

> The management API's transport defaults are part of this decision: CORS is an exact-match allowlist
> defaulting to empty (never `*`, never a reflected `Origin`), and the default bind address is
> `127.0.0.1`. **W5 §3.5 owns both.** The public website's bind address is unaffected.

**Status:** proposed, not applied. §3.5 designs under the proposed behaviour and records it as
Decisions 30 and 31, so the change is visible if the amendment is rejected.
