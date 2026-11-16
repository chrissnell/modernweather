# The diff harness — both daemons, same inputs, every difference predicted

`docs/implementation-plan.md` **M5-2**. Two commands, and the answer to "how do I test this
thing" is: run them and read the summary.

| | What it diffs | Command |
|---|---|---|
| **(b)** | same station, both daemons, diffed **readings** | `mw-diff readings` |
| **(c)** | same database, both read paths, diffed **responses** | `mw-diff responses` |

Mechanism **(a)** is the conformance suite and has run since M1. It diffs the *contract* —
status, headers, key set, value types, numeric formatting — against either binary, and it is
deliberately **value-blind** (W1 D-3), because values change every two seconds. That
blindness is exactly what these two cover: (a) cannot catch a wrong *number*.

## Lifetime: this is cutover apparatus, and it is meant to be deleted

**Owner's scoping, 2026-11-13:** *"checking that they match is really only something we have
to do once as we transition suncrestweather.com to the new service. once we do that,
remoteweather goes into the dustbin of time."* That is recorded here rather than left on an
issue because this directory is the kind of thing that gets inherited by default.

So: the two mechanisms exist to be run **once per station**, immediately before cutover, and
then never again. Nothing in the daemon depends on this crate, no gate but its own runs it, and
when the Go daemon is decommissioned the correct action is `git rm -r ci/diff` together with
the `diff-harness` entry in `ci/gates.toml`, its job in `.github/workflows/pr.yml` and its row
in `docs/release.md` §1. A gate whose subject no longer exists reports green forever and
guards nothing, which is the same failure as a gate that reports green by deferral
(`docs/reconciliation.md` G-1) wearing the opposite costume.

**What must not be deleted with it** is anything the harness only *found*. A defect in the new
service that this directory happens to be the first thing to notice belongs in a work item, in
the deviation ledger, or in a test next to the code — not in a README that is scheduled for
removal. The findings from the first run are at the bottom of this file for that reason: they
are a hand-off list, not a record to keep.

---

**The expected-diff set is `expected.toml`, and it is data.** Every difference the harness
finds is matched against a rule there; a difference that matches no rule is **unexpected** and
the run is red. There is no threshold to argue about after the fact and no "ignore" — the
rules are *predicates*, so `DEV-4` is an arithmetic bound that fails when the two sides differ
by more than the Go accumulator's own worst case, and `DEV-7` asserts the new value is the old
one multiplied by the bucket's reading count. A rule can fail on a key it names, which is the
difference between a prediction and an exemption.

---

## Running it

```sh
cd ci/diff && cargo build          # a standalone crate: no Rust daemon needed for (b)
./target/debug/mw-diff help
```

Both mechanisms take **both** sides as parameters, for `DESIGN.md` R-3's reason — the
conformance runner takes a base URL precisely so it can target either implementation, and so
does this. Nothing here records a particular installation: no hostname, address, device path
or credential is in this repository, and an operator's actual values belong wherever that
machine's configuration is kept.

### (b) readings

Both daemons receive the same frames — a WeatherLink Live station broadcasts over UDP, and a
serial station broadcasts through the `ser2net` fan-out M5-1a measured (`ci/serial-bridge/`).
Each writes to its own database, so this compares two tables.

```sh
mw-diff readings \
  --go   "psql:<whatever invocation reaches the Go daemon's database>" \
  --rust "psql:<whatever invocation reaches the new daemon's database>" \
  --go-station CSI --rust-station csi \
  --since 2026-11-13T18:00:00Z --until 2026-11-14T18:00:00Z
```

`psql:` takes the **whole invocation**, which is what `PSQL_SOURCE` / `PSQL_TARGET` are in
`ci/db/migrate-from-go/migrate-from-go.sh` and for the same reason: neither database is
reachable by a bare `psql` from the same machine, and one of them is in a pod. Split on
whitespace, no quoting — a password goes in the environment of the command, and the report
redacts anything that looks like one.

Columns are read from the two schemas at run time rather than listed here. The 215 columns
belong to `metrics/registry.toml` and the generator; a third copy would drift.

### (c) responses

```sh
mw-diff responses \
  --go   "https://<the Go server>" \
  --rust "http://<the new daemon>" \
  --station CSI --rust-station csi \
  --census      "psql:<the Go daemon's database>" \
  --census-rust "psql:<the new daemon's database>"
```

**The census is not optional in practice.** DEV-4's bound is `n × 2⁻²⁴` relative and DEV-7's
relationship is "× n", where n is the bucket's reading count — and no `/span` response carries
it. Without `--census` every bucket those rules apply to is reported as `no_census`, which is
**not** clean: a tolerance whose n is unknown is not a tolerance. Without `--census-rust` an
unevenly populated bucket cannot be told from a value difference, and that one matters more
than it sounds — see the preconditions below.

### The negative control

```sh
./self-test.sh          # the `diff-harness` gate's command
```

W1 **D-10**, which M5-2 restates: *a differ that cannot be made to differ proves nothing.*
`self-test.sh` drives both mechanisms over `recorded/` — a clean run, then the same run with a
1% error injected — and asserts the first is green, every injection is red and names the
metric, an injection that moved **nothing** is refused rather than passed, and a run with no
census reports `no_census`. It needs no database, no network and no production, which is why
it is a CI gate and the two mechanisms are not.

`recorded/go/` is a **capture** of the live Go server; `recorded/rust/` and the two censuses
are **derived** from it by `recorded/build.py`, which applies exactly the deviations
`expected.toml` predicts and nothing else. That is what makes a clean run green *by
construction* and a red run attributable to the injection — a corpus captured from two live
deployments could not do it, because the deployments differ in their configuration and the
harness correctly reports that. `build.py --check` is run by `self-test.sh`, so a corpus
edited by hand is a failure rather than a surviving change.

`--inject <key>:<percent>[:go|rust]` is available on both mechanisms against live systems too.
It is applied after both sides are read and before the comparison, so it passes through the
real pairing, the real flattening and the real rule set.

---

## Reading the summary

Three parts, in this order. A **provenance block** naming the two systems the verdict is
about, because a verdict with no subject is not a result. A **table**, one row per unit of
comparison — one per metric for (b), one per endpoint and band for (c). Then a **loud block**
per failure with a worked example in it, and one verdict line.

Two conventions worth knowing. **Expected differences are printed, not hidden**: `DEV-4:90` on
a line means that rule fired ninety times, and a rule that *stopped* firing is as interesting
as one that started — a `/span` line that used to read `DEV-7:126` and now reads nothing is
DEV-7 reverted, which the harness shows as a count rather than as a failure (`solarjoules` is
zero all night and a mandatory rule would turn every nocturnal bucket red). And **a count of
zero is printed as `0`**, because a blank cell reads as "not checked" and the whole claim here
is that everything was.

Columns that are not value comparisons:

| | |
|---|---|
| `paired` | readings or buckets actually compared |
| `uneven` | buckets present on both sides that were fed **different numbers of readings**. Their values are *not* compared — every statistic in such a bucket differs, and thirteen reported value differences would bury the one fact that explains them |
| `go-only` / `rust-only` | coverage. The two databases do not hold the same readings over the window |
| `unpaired` / `ambiguous` | (b) only. A reading with two candidate partners inside the tolerance is reported, never resolved by taking the nearer — silently choosing would mis-pair a window in which one daemon dropped a frame and then every metric differs for a reason that has nothing to do with either implementation |

An endpoint that compared **no** keys is never green, even when both sides answered `200`: a
`/span` that returned `[]` on both sides, or seven endpoints all answering `404 station not
found`, is an agreement about the absence of data and not an agreement about data. The one
exception is a matching **non-2xx** status — `/snow` 404s for a website with snow disabled, and
the two sides agreeing on the status and the error body is a real result.

The window is loaded wider than it is compared, by the pairing tolerance. A frame received at
`17:59:59.999` by one daemon and `18:00:00.002` by the other has to have both halves available
or it reads as a missing reading at every window edge.

---

## Preconditions, and why they are the first thing to check when a run is red

The harness reports differences. Two of its inputs can make every difference meaningless, and
neither is a defect in either implementation:

1. **The two daemons must be configured equivalently.** A per-station calibration present on
   one side and not the other produces a difference in every reading. Measured on the first
   production run: a `wind_dir_correction` of −94° configured on the Go side and absent on the
   new side made `winddir` differ on 100% of readings, `windd` and `windcard` differ on 100%
   of `/span` buckets, and nothing else. The harness names it — "rust − go = +94 modulo 360 on
   every differing reading: a bearing offset configured on one side and not the other" —
   because a constant offset is the signature of exactly this and reading 600 identical lines
   to notice it is work the harness can do. Station **coordinates** are the other one: they
   move `potentialsolarwatts`, `sunrise`, `sunset` and the moon keys, and DEV-9's own
   measurement is that the two implementations agree to 1.1 × 10⁻¹² W/m² when the
   configuration matches.
2. **For (c), both databases must hold the same readings over the window.** `/span`'s window
   always ends *now*, so a new database that is behind the source produces `go-only` buckets
   and `uneven` ones, and the values that did pair are then the only part of the line that is
   a comparison. The harness says so on the line rather than averaging it in.

A run that reports `go-only`, `uneven` or a constant offset has not yet reached the question
M5-2 asks. Fix the input and run it again.

---

## The expected-diff set

`expected.toml` is the file; its own comments are the argument for each rule. The rules as of
M5-2:

| Rule | Where | The predicate, which is the prediction |
|---|---|---|
| **D-26** | readings | the Go side wrote exactly `0.0` and the new side wrote NULL. Not the reverse, and not a real value against NULL |
| **DEV-9** | readings | `windchill` / `heatindex`: the new side is absent **and** the Go side holds this reading's `outtemp`, which is what the Go drivers store where the formula does not apply |
| **DEV-4** | `/span` | `period_rain` and `windd` within `n × 2⁻²⁴ × max(\|a\|,\|b\|,1)` — the same model and the same constant `w9.rollup_report` reconciled the migration with |
| **DEV-7** | `/span` | `solarjoules` on the new side is the Go side's value **times the bucket's reading count** |
| **DEV-12** | `/span` | at most one extra bucket, at the **leading edge** only |
| **DEV-11** | `/span`, `/latest` | the Go side's **body** is not JSON and carries a non-finite literal while the new side's parsed. A body rule, not a value rule: `encoding/json` refuses a non-finite mid-stream and truncates, so there is no pair of values to compare |

Adding a rule is code **and** data: the `class` is a predicate in `src/expected.rs`, so a new
one is a function somebody reviews rather than a line somebody adds. The loader refuses an
unknown class, a `null_vs_zero` with no declared direction, a `*` alongside named keys, a
value rule that names nothing, and a schema it does not read — each of those is asserted in
`tests/control.rs`, because an expected-diff set that half-loaded would make a run green by
losing the rule that would have failed it.

**A rule cites a decision.** `id` is an entry in `docs/reconciliation.md` or `docs/DESIGN.md`.
A rule with no decision behind it does not belong: the whole claim of this milestone is that
every difference between the two implementations was predicted in advance *from a decision*,
and a rule invented to make a red run green is the failure mode the file exists to prevent.
When the harness reports something real that no decision predicts, the fix is a deviation
entry in the ledger and then a rule — in that order.

---

## What the first production run found, 2026-11-13

Run from an agent runtime against the live Go server and `pitcairn`'s database as the oracle,
and the new daemon and M5-1's migrated database as the subject. Both halves ran; neither meets
its done-criterion yet, and the reasons are inputs rather than arithmetic.

**(b)** 480 readings over 40 minutes of one Campbell station, paired **480/480** through the
`ser2net` bridge with a largest pairing offset of **1 ms**, 197 numeric columns compared.
Result: **184 columns differ only as D-26 predicts**, `windchill` and `heatindex` only as
DEV-9 predicts, nine exactly equal — and **two columns differ for reasons no rule predicts**,
both of them the two deployments' configuration rather than either implementation: `winddir`
by the −94° vane calibration below, and `potentialsolarwatts` by the station's coordinates.
M5-2's criterion is a 24-hour run reporting **only** NULL-versus-zero; the window here is
short because that is how long both daemons had been receiving the same station, and the
criterion cannot be met until the calibration and the coordinates match.

**(c)** All four `/span` bands, `/latest`, `/snow` and `/almanac`. **DEV-4 fired 148 times and
DEV-7 353**, inside their bound and matching their relationship, which is the first evidence
either deviation behaves as predicted on real data. Beyond the two configuration differences
above and the coverage gap (the migrated database was ~19 hours behind the source, which the
harness reported as `go-only` and `uneven` buckets), five things no decision predicts:

| | |
|---|---|
| **`stationtype`** | `"campbell"` on the Go side, `"campbellscientific"` on the new side, on every `/span` element. The device type string in the two configurations |
| **1-ulp `float32` differences** | on `ohum`, `otemp`, `bar`, `extratemp1`, `solarwatts`, `stationbatteryvoltage` and `windch`, in a small minority of buckets. In every case examined the two sides hold the **identical** `float8` value and it lies **exactly** on a `float32` tie point: e.g. `observations_1h` gives `38947.830963134766 / 720`, which is precisely the midpoint of two adjacent `float32`s; the Go server emits the upper neighbour and the new side rounds half to even and emits the lower. A serialisation tie-break, contract-visible in the last digit, and **not in the deviation ledger**. The conformance suite does not catch it because no fixture happens to sit on a tie |
| **`/span`'s `rainincremental`** | `0` on the Go side against `0.04` on the new side, on nine of twenty-nine daily buckets. The Go daily view carries no value here and the DTO zero-fills it; the new side serves the stored statistic |
| **`/latest`** | `stationname` is `""` and `stationtype` is absent on the new side where the Go side sends `"CSI"` and `"campbell"`; `monthrain` and `yearrain` are `0` where the Go side sends 1.54 and 15.11 inches. **The first half is fixed by M5-5**: both values were NULL in the migrated rows, and the read path no longer looks there — `stationname` comes from `config.stations.name` and `stationtype` from `stations.reported_type`. Re-check it on the next (c) run against a daemon built from that change and strike it if it is gone. `monthrain`/`yearrain` are untouched and still open |
| **`/almanac`** | `{}` on the new side — fourteen records against none, so the endpoint compared nothing. The migration carries no `almanac_cache` and nothing has populated it. Two consequences, and the second is a cutover decision rather than a defect: the cache has to be built before the endpoint serves anything, and the Go cache holds records from **2025-08-19** — earlier than the first imported reading of 2025-10-30 — so those all-time extremes are not reproducible from the migrated data and would reset at cutover. **Resolved by M7-6 and the finding was bigger than this row**: nothing in the program wrote that table at all (A-M7-6-1), and the Go cache turned out to be the stale one — see below |

`/snow` is the only endpoint green on both sides, and it is green because both return
`404 snow data not enabled for this website`. That is a real agreement and the harness says
which one it is.

**The proposed deviation for the tie-break**, so that it is one edit rather than an
investigation: a class `float32_tie_break` whose predicate is "the two literals are adjacent
`float32` neighbours", scoped to `/span` and `/latest` with `keys = ["*"]`. It is deliberately
**not** in `expected.toml`: the ledger entry has to exist first, because a rule with no
decision behind it is the thing this file is designed to make impossible.

## What M7-6 found about `/almanac`, 2026-11-14

The row above read the empty `/almanac` as a migration consequence. It is not: **nothing in
the program wrote `almanac_cache`** (`docs/reconciliation.md` **A-M7-6-1**), and once
`almanac:records` was computing the twelve records from the migrated history, the comparison
against the live Go server became possible for the first time. It was done by evaluating each
record's query against the preview's database and comparing the result with
`GET https://suncrestweather.com/almanac?station=CSI`, taken a minute apart.

**The headline: the Go server's almanac is the stale one.** Four of its records are beaten by
days that are *in its own history* and it has not noticed, which fixes a lower bound on how
long its hourly job has been failing or unscheduled: `high_temp` is **95.73979 °F on
2025-08-19**, and the same history holds **100.3874 °F on 2026-07-12**. A `refresh_almanac_cache`
that had run since that day would have replaced it, because its first statement is
`DELETE FROM almanac_cache WHERE stationname = $1`. So **the Go cache has not been successfully
refreshed since at least 2026-07-12** — four months. Its mixed dates say the same thing from
the other side: the newest timestamp anywhere in it is 2026-05-18, and `high_temp`'s 2025-08-19
is now outside `weather_1d`'s own one-year window.

Record by record, against the station's own history:

| Record | Go serves | Computed here | |
|---|---|---|---|
| `low_temp` | 12.097401 @ 2026-02-18T17:00−07 | 12.097401 @ 2026-02-19T00:00Z | **equal**, and the same instant — the contract renders the UTC day bucket in the station's offset |
| `high_solar` | 1514.0381 @ 2026-05-18T13:04:28.657385−06 | 1514.0381, same reading | **equal**, including the sub-second instant, which is what the two-step raw lookup is for |
| `high_wind_speed` | 46.014404, `wind_dir` **184.01839** | 46.014404, `wind_dir` **184.01834** | the gust is equal; the direction differs in the **seventh significant digit**. Both are a circular mean over the same day's readings, accumulated in a different order — the same class as the `float32` tie-break row above, one function further in |
| `high_pm25` | 731.13 @ 2026-01-19T17:00−07 | 731.13 @ 2026-01-20T00:00Z | **equal** |
| `high_pm10_in` | 655.33, same instant | 655.33, same instant | **equal, and read from `extrafloat2`** — `pm10_in_aqin` returns no row at all for this device, which is migration 030's finding ("PM10 was always blank for AirGradient devices") confirmed against production data |
| `high_co2` | 646 @ 2026-02-27T17:00−07 | 646 @ 2026-02-28T00:00Z | **equal** |
| `high_tvoc` | 500 @ 2025-09-28 | 500 @ 2025-12-11 | **equal in value, different day.** 500 is the TVOC index's ceiling, so the record is a tie among many days and the earliest one Go holds predates the migrated history's first reading (2025-10-30) |
| `high_temp` | 95.73979 @ 2025-08-19 | **100.3874 @ 2026-07-12** | Go is stale. The new value sits on a smooth distribution of July maxima (99.03, 98.62, 98.34, …), so it is weather and not a glitch |
| `low_humidity` | 6.698 @ 2025-08-03 | **6.541 @ 2026-06-26** | Go is stale |
| `max_rain_hour` | 0.23000003 @ 2026-04-28 | **0.53999996 @ 2026-08-14** | Go is stale. **Also the one record with a retention caveat**: `observations_1h` is kept for 90 days, so this is the record `almanac:records` carries forward rather than lowering (A-M7-6-1) |
| `max_rain_day` | 0.90999943 @ 2025-10-03 | **0.94 @ 2026-10-12** | Go is stale |
| `low_barometer` | 29.440966 @ 2026-02-17 | **6.147712 @ 2026-10-20** | **a data defect, not an implementation one — and it is user-visible.** The history holds thirteen consecutive readings of 6.147712 inHg over ninety seconds on 2026-10-20, which is not a pressure. Go's `> 0` filter does not exclude it either, so a Go server whose job was running would serve the same number; Go is spared only because its cache predates the glitch. The right fix is upstream of this endpoint — either those thirteen rows go, or ingress gains a plausibility range — and neither belongs to M7-6, which would have to invent a threshold the frozen contract does not have |

**Two of these are worth carrying into the cutover conversation.** The almanac is the first
endpoint where the **new** implementation is demonstrably more correct than the live one, so a
cutover changes four visible numbers on that page for the better — and `low_barometer` changes
one for the worse, from a stale-but-plausible 29.44 inHg to a current-and-absurd 6.15, until
the thirteen rows or an ingress check deal with it.
**One difference the table above does not list, because it applies to every row of it: the
offset.** Go renders each `timestamp` in its database session's zone and this side renders it
in UTC — `"2026-01-19T17:00:00-07:00"` against `"2026-01-20T00:00:00Z"`, which is the **same
instant** written two ways. It is observable for the first time here, because `/almanac` is
the only endpoint on this surface that marshals a `time.Time` at all and it has never had data
to marshal.

**No code change, and the reason is the contract rather than a preference.**
`contract/fixtures/almanac-populated.json` was captured from a Go server and records
`"2026-10-28T00:00:00Z"` — the capture rig's own session was UTC — so `Z` is what the frozen
fixture says and what `mw_api_legacy::dto::go_rfc3339` emits. The production server's offset is
a property of *its* session zone, not of the contract, and a client cannot tell the two apart:
both are RFC 3339 and both parse to one instant. What would be wrong is to make the rendering
depend on a database session's zone, which is how Go came to have two answers.

**It belongs in `expected.toml` as a class rather than as twelve rows** — predicate "the two
literals are RFC 3339 timestamps denoting the same instant", scoped to `/almanac` with
`keys = ["*.timestamp"]`. It is deliberately not added here yet, for the same reason the
`float32_tie_break` class above is not: the ledger entry has to exist first.
