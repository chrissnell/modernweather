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
| **DEV-11** | `/span`, `/latest` | the Go side emitted a non-finite literal and the new side emitted exactly `0` |

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

**(b)** 600 readings over 50 minutes of one Campbell station, paired 600/600 through the
`ser2net` bridge with a largest pairing offset of **7 ms**, 197 numeric columns compared.
Result: **184 columns differ only as D-26 predicts**, `windchill` and `heatindex` only as
DEV-9 predicts, nine columns exactly equal — and **two columns differ for reasons no rule
predicts**, both of them the two deployments' configuration rather than either
implementation: `winddir` by the −94° vane calibration above, and `potentialsolarwatts` by the
station's coordinates. M5-2's criterion is a 24-hour run reporting **only** NULL-versus-zero;
the window here is 50 minutes because that is how long both daemons had been receiving the
same station, and the criterion cannot be met until the calibration and the coordinates match.

**(c)** All four `/span` bands, `/latest`, `/snow` and `/almanac`. DEV-4 fired **149** times
and DEV-7 **314**, inside their bound and matching their relationship, which is the first
evidence either deviation behaves as predicted on real data. Beyond the two configuration differences above and the coverage gap
(the migrated database was ~19 hours behind the source, which the harness reported as
`go-only` and `uneven` buckets), four things no decision predicts:

| | |
|---|---|
| **`stationtype`** | `"campbell"` on the Go side, `"campbellscientific"` on the new side, on every `/span` element. The device type string in the two configurations |
| **1-ulp `float32` differences** | on `ohum`, `otemp`, `bar`, `extratemp1`, `solarwatts`, `stationbatteryvoltage` and `windch`, in a small minority of buckets. In every case examined the two sides hold the **identical** `float8` value and it lies **exactly** on a `float32` tie point: e.g. `observations_1h` gives `38947.830963134766 / 720`, which is precisely the midpoint of two adjacent `float32`s; the Go server emits the upper neighbour and the new side rounds half to even and emits the lower. A serialisation tie-break, contract-visible in the last digit, and **not in the deviation ledger**. The conformance suite does not catch it because no fixture happens to sit on a tie |
| **`/latest`** | `stationname` is `""` and `stationtype` is absent on the new side where the Go side sends `"CSI"` and `"campbell"`; `monthrain` and `yearrain` are `0` where the Go side sends 1.54 and 15.11 inches; `windgust` differs by about 1 mph |
| **`/almanac`** | `{}` on the new side. The migration carries no `almanac_cache` and nothing has populated it. Two consequences, and the second is a cutover decision rather than a defect: the cache has to be built before the endpoint serves anything, and the Go cache holds records from **2025-08-19** — earlier than the first imported reading of 2025-10-30 — so those all-time extremes are not reproducible from the migrated data and would reset at cutover |

`/snow` is the only endpoint green on both sides, and it is green because both return
`404 snow data not enabled for this website`. That is a real agreement and the harness says
which one it is.

**The proposed deviation for the tie-break**, so that it is one edit rather than an
investigation: a class `float32_tie_break` whose predicate is "the two literals are adjacent
`float32` neighbours", scoped to `/span` and `/latest` with `keys = ["*"]`. It is deliberately
**not** in `expected.toml`: the ledger entry has to exist first, because a rule with no
decision behind it is the thing this file is designed to make impossible.
