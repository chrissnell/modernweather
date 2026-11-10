//! `cargo xtask conformance` — replay the frozen legacy REST fixtures against the **Rust**
//! binary.
//!
//! This is the second half of `DESIGN.md` R-3. The first half, `ci/conformance/go-target.sh`,
//! replays the same 70 fixtures against the **Go** daemon and proves they describe reality;
//! read it before changing anything here, because the two halves are deliberately the same
//! shape. The difference is the target: `legacy-conformance` takes a base URL precisely so
//! that one runner can be pointed at either server (W1 Decision 7).
//!
//! What a run does, in order:
//!
//! 1. preflight the tools and the two ports this run must own;
//! 2. build `modernweather`, recreate the weather database from `migrations/` and **seed
//!    it** from `ci/conformance/rust-seed/` -- the stations, the readings, the aggregate
//!    refresh and the three caches the frozen endpoints read directly;
//! 3. bootstrap a SQLite configuration database by starting the binary once, seed it with
//!    the listener, the weather database and W1's seven stations and six websites, start
//!    the binary for real, and wait for `snapshot:warm` to have run;
//! 4. build `contract/conformance` with `--locked` and replay all 70 fixtures at it;
//! 5. judge the result, and — only if the replay is clean — run W1 Decision 10's negative
//!    control through `contract/tools/rig/mangle_proxy.py`.
//!
//! **What a green run means, which is not "every fixture passed".** The 70 fixtures are
//! classified (`docs/reconciliation.md` §6.4, work item **N-18**) and the classification is
//! **data the runner reads** rather than a narrower replay -- all 70 are still attempted, and
//! the `attempted == 70` assertion below is what stops the classification arriving as a
//! silent filter. What this command does with it is the **policy**:
//!
//! * **`frozen`** -- the five endpoints the iOS client calls. Every one must pass. A failure
//!   here is a contract break.
//! * **`coupled`** -- the website's. Every one must pass **except** the ones in [`PENDING`],
//!   whose subject no landed work item implements; that list is declared here by name and
//!   checked in **both** directions, so a fixture excused without a declaration and a
//!   declaration whose fixture has started passing are each exit 2.
//! * **`retired`** -- `/alerts`, `/api/remote-stations` and `format=msgpack`, which **D-69**
//!   does not carry forward. Reported, never gating.
//!
//! **DEV-10 is passed to the runner**, because the Rust target is known to differ from the
//! recorded Go response on two `/stationinfo` keys on purpose
//! (`docs/reconciliation.md` O-5, the owner's answer). The fixture holds both halves and
//! `ci/conformance/go-target.sh` passes no deviation, so each target is measured against
//! the bytes it is supposed to produce.
//!
//! Two exit codes, and the distinction is the point (the same reasoning as W2 D-18, which
//! `ci/gates.toml` records as C-W2-20):
//!
//! * **1 — the replay reached a verdict and the binary does not conform.** Fixtures
//!   failed. This is the expected state until M4-2.
//! * **2 — no verdict was reached, so the run proved nothing.** A missing tool, a port
//!   already in use, a `$PGDATABASE` this run will not drop, a database that would not
//!   answer, a daemon that would not start, a fixture count that is not 70, a replay where
//!   everything skipped, failures at the transport layer, or a negative control that did
//!   not hold. A suite that cannot be made to fail proves nothing, and neither does one
//!   whose target was not listening — both are this code, never 1.
//!
//! **What this does NOT decide.** M4-2 narrowed the frozen surface to the five endpoints
//! iOS calls (`docs/reconciliation.md` §6): `/alerts`, `/api/remote-stations` and
//! `format=msgpack` are not being implemented, so their fixtures will still fail when the
//! rest are green. Classifying the 70 fixtures into frozen, coupled and not-implemented is
//! M4-2's work, not this item's — this runner replays the whole set and says so, and the
//! `attempted == 70` assertion below is what stops that classification from arriving as a
//! silent filter.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

/// Every fixture W1 froze. Asserted as an equality rather than a floor: a fixture set that
/// shrank is a contract that shrank, and the one thing this item's done-criteria ask for is
/// that the runner reports 70 fixtures *attempted*.
const EXPECTED_FIXTURES: usize = 70;

/// Where the daemon under test listens.
///
/// Not a free ephemeral port, and not the schema's 8080: `contract/tools/rig/mangle_proxy.py`
/// hard-codes `TARGET = ("127.0.0.1", 18080)` and listens on 18081. The negative control is
/// part of this run's protocol, so the daemon goes where the proxy already looks. That file
/// is W1's frozen tooling; moving the daemon is cheaper than editing it.
const REST_PORT: u16 = 18080;
/// Where `mangle_proxy.py` listens. Also hard-coded there.
const MANGLE_PORT: u16 = 18081;

/// How many fixtures the negative control must **break** -- not how many must fail.
///
/// **The shape of this assertion is re-derived, and that matters more than the number.**
/// `contract/README.md`'s 26 is a count of *failures* against the Go daemon, which serves
/// all 70 endpoints and passes all 70 directly -- so against that target "26 failed" and
/// "the proxy broke 26" are the same sentence. Against this one they are not: 22 fixtures
/// fail whatever the proxy does, because nine are `retired` and thirteen are [`PENDING`].
/// A floor on the failure count would therefore be cleared by a proxy that mangled
/// **nothing**, which is exactly the failure mode W1 Decision 10 exists to prevent.
///
/// So the control compares against the **direct** replay: `direct.passed - control.passed`
/// is the number of responses the proxy actually broke, and it is independent of how many
/// endpoints the target does not serve.
///
/// **Measured at 23**, against a green direct replay of 48:
///
/// ```text
/// direct   48 passed, 22 failed
/// mangled  25 passed, 45 failed      -> 23 broken
/// ```
///
/// The 23 are every JSON-object-or-array response on an implemented endpoint -- the six
/// `/latest`, six `/span`, five `/snow`, three `/stationinfo`, two `/almanac` and one
/// `/api/stations`. The 25 that still pass are the plain-text error bodies, the `403`, and
/// the whole `/forecast` family: the proxy rewrites parsed JSON and `/forecast`'s body is
/// `jsonb` text it does not match. That split is what makes the control meaningful rather
/// than a blanket failure.
///
/// A **floor** rather than an equality, for `go-target.sh`'s reason: the proxy's `windd`
/// rewrite only matches an integer-formatted value, so the exact count varies with live
/// data. Twenty leaves three fixtures of slack and is still twice what any single rewrite
/// accounts for.
const MIN_MANGLED_BROKEN: usize = 20;

/// The same floor, over the `frozen` class alone.
///
/// **Measured at 20** on the same run: every one of the 23 broken responses bar three
/// `/almanac` and `/api/stations` bodies is a frozen one. Fifteen leaves five of slack, the
/// same proportion the total floor carries.
///
/// This exists because the total floor stops binding on its own. Six coupled fixtures pass
/// directly today, so a total of 20 cannot be reached without the frozen ones -- but
/// `implementation-plan.md` **M4-3** makes all 19 coupled pass, and 19 + 1 would then clear
/// it while the control said nothing about the five endpoints `D-68` freezes. A floor that
/// only holds until the next work item is not a floor.
const MIN_FROZEN_MANGLED_BROKEN: usize = 15;

/// The deviation this target is known to differ by on purpose, and **which fixtures may
/// declare it**.
///
/// `docs/reconciliation.md` **O-5**, answered by the owner as option (b): `/stationinfo`
/// emits `about_text` and `weather_device` unconditionally, because the iOS client declares
/// both properties non-optional and an absent key fails the decode of the whole object. The
/// two affected fixtures carry a `deviation` block holding the Rust target's response beside
/// the Go server's, so `ci/conformance/go-target.sh` keeps asserting what shipped.
///
/// The fixture list is the point, and it is the same reasoning as [`PENDING`]'s. A
/// deviation is "compare against bytes the author wrote instead of the bytes the Go server
/// produced", so an id enabled target-wide is an unbounded excuse: a `deviation` block
/// added to `latest-full-complement` would be applied, would print `ok`, and would turn the
/// 133-key DTO into whatever its author typed. Naming the fixtures here makes widening it a
/// reviewable line in the gate, and [`judge_direct`] asserts the set the runner actually
/// applied it to equals this one.
const DEVIATIONS: [(&str, &[&str]); 1] = [(
    "DEV-10",
    &[
        "stationinfo-about-text-omitted",
        "stationinfo-no-weather-device",
    ],
)];

/// Every `frozen`-class fixture, by name.
///
/// **The class is per-fixture JSON, so on its own it is a filter anybody could widen by
/// editing one file** -- the same objection [`PENDING`]'s own documentation raises about the
/// `pending` field, and it applies with more force here, because moving a fixture out of
/// `frozen` is what stops it gating at all. The `cargo test` pin on the class *totals* does
/// not close it: a compensating move keeps 42/19/9 exact.
///
/// So the gate carries the roster and [`judge_direct`] asserts every reported fixture's
/// class against it. A reclassification is then a line in this file, next to the policy it
/// changes, rather than a one-word edit in a fixture.
const FROZEN: [&str; 42] = [
    "forecast-24-hourly-periods",
    "forecast-240-ten-daily-periods",
    "forecast-by-station-name",
    "forecast-empty-data",
    "forecast-period-not-a-number",
    "forecast-period-out-of-range",
    "forecast-single-period",
    "forecast-span-not-found",
    "forecast-span-too-many-digits",
    "forecast-station-name-not-found",
    "forecast-station-not-found",
    "latest-air-quality-device",
    "latest-by-station-id",
    "latest-default-station",
    "latest-full-complement",
    "latest-invalid-station-id",
    "latest-minimal-complement",
    "latest-no-recent-data",
    "latest-snow-device-offline",
    "latest-snow-device",
    "latest-station-id-not-found",
    "latest-station-not-found",
    "snow-by-station-id",
    "snow-device-never-reported",
    "snow-device-stale",
    "snow-disabled-for-website",
    "snow-reporting",
    "snow-station-param-overrides-depth-only",
    "span-1h-one-minute-buckets",
    "span-24h-five-minute-buckets",
    "span-720h-daily",
    "span-72h-hourly-with-gap",
    "span-day-unit-rejected",
    "span-empty",
    "span-exceeds-one-year",
    "span-invalid-duration",
    "span-missing-station",
    "span-snow-device-with-station-id",
    "span-station-not-found",
    "stationinfo-about-text-omitted",
    "stationinfo-full",
    "stationinfo-no-weather-device",
];

/// Every `retired`-class fixture, by name. [`FROZEN`]'s reasoning, in the direction that
/// matters more: `retired` never gates, so a fixture moved into it stops being checked.
///
/// `/alerts`, `/api/remote-stations` and `format=msgpack` -- the three subjects
/// `docs/reconciliation.md` §6.3 does not carry forward, which **D-69** disposes of by
/// deleting them rather than by porting them.
const RETIRED: [&str; 9] = [
    "alerts-active",
    "alerts-empty",
    "alerts-invalid-station-id",
    "alerts-missing-station-id",
    "api-remote-stations-forbidden",
    "api-remote-stations-populated",
    "forecast-msgpack",
    "latest-msgpack",
    "snow-msgpack",
];

/// The fixtures whose subject no landed work item implements, and who owns each.
///
/// **Empty, and that is implementation-plan.md M4-3's visible done-criterion on this gate.**
/// It held thirteen entries -- the six HTML pages and the seven embedded assets, every one
/// of them `mw-web`'s -- excused because no landed item served them. M4-3 ported the three
/// templates to `askama` with a `GoHtmlEscaper`, embedded the assets with a hard-coded mime
/// table, and made the acceptance test a byte diff against those same recorded bodies; all
/// nineteen `coupled` fixtures pass, which is Milestone 4's done-criterion, and the list
/// emptying is how this gate says so.
///
/// **Coupled-class only, for whatever lands here next.** Every entry was a website page or
/// an embedded asset. A `frozen` fixture must never appear here: that class is the five
/// endpoints **D-68** freezes, the iOS client is deployed independently of this server, and
/// an excuse there is a contract break with a note attached. [`judge_direct`] enforces the
/// class separately, so this is a rule about what to *write* rather than one it has to check.
///
/// **Declared here rather than read off the fixtures**, and checked in both directions. A
/// fixture may carry a `pending` field saying the same thing, and that field alone would be
/// a filter anybody could widen by editing one JSON file: the list that *excuses* a failure
/// lives in the gate, next to the policy, and a disagreement between the two is exit 2.
/// With the list empty, the matching check is that **no** fixture carries a `pending` field
/// -- which is why M4-3 removed thirteen of them in the same commit.
const PENDING: [(&str, &str); 0] = [];

/// The schema's seeded `listener.http_port`./// The schema's seeded `listener.http_port`. The bootstrap start below binds this, because
/// the row it is about to be given is the one that says otherwise.
const SCHEMA_PORT: u16 = 8080;

/// Where the seed lives. Applied in filename order, which is load-bearing: the aggregate
/// refresh has to follow the readings and the caches have to follow the `stations` rows they
/// reference.
const WEATHER_SEED: &str = "ci/conformance/rust-seed";

/// Every station the rig configures, and whether it must have readings.
///
/// The `false` entries are as load-bearing as the `true` ones: `nolocation` is
/// `latest-no-recent-data`, `span-empty` and `almanac-empty`, and `snowgauge-dead` is
/// `snow-device-never-reported`. A seed that gave either of them a reading would turn three
/// `404`s and an empty array into `200`s and read as a contract failure.
///
/// `snowgauge-stale` is `true` and its readings are deliberately **three days old**, which
/// is the pair `latest-snow-device-offline` (404, because `/latest` has a recency rule) and
/// `snow-device-stale` (200, because `/snow` has none).
const SEEDED_STATIONS: [(&str, bool); 7] = [
    ("davis-full", true),
    ("csi-minimal", true),
    ("snowgauge", true),
    ("aqmon", true),
    ("snowgauge-stale", true),
    ("nolocation", false),
    ("snowgauge-dead", false),
];

/// The configuration seed, applied to the SQLite database between the bootstrap start and
/// the real one.
///
/// Separate from the four above because it is a different database and a different tool:
/// `sqlite3` rather than `psql`, and it has to run while the daemon is **stopped**.
const CONFIG_SEED: &str = "ci/conformance/rust-seed/01-config.sql";

/// The message `snapshot:warm` logs when it has finished seeding the snapshot table.
///
/// Waited for before the replay, and the reason is a race rather than tidiness:
/// `snapshot:warm` is an `OnStartup` component with a hard `ReadingStore`, so it runs after
/// `db:postgres` has connected -- which is after the daemon is answering on `/metrics`. A
/// replay that started first would read a `404` from `/latest` for every station and report
/// it as a contract failure.
///
/// A const for [`DB_ASSERTED`]'s reason: a reword of that log line would otherwise break
/// this gate 60 seconds into a CI run rather than in `cargo test`.
const SNAPSHOT_WARM: &str = "the snapshot table is warm";

/// The message `crates/modernweather/src/boot.rs` logs on the success path of **D-62**, and
/// the only evidence available that the daemon reached the database this command prepared.
///
/// A const rather than a literal at the one use site so that a test can assert the daemon
/// still logs it: this is the one cross-file coupling here that a reword would break
/// silently, and it would break it 60 seconds into a CI run rather than in `cargo test`.
const DB_ASSERTED: &str = "database version asserted";

pub fn main(args: &[String]) -> Result<ExitCode, String> {
    let root = crate::repo_root();
    let Some(opts) = Options::parse(&root, args)? else {
        return Ok(ExitCode::SUCCESS);
    };

    preflight(&opts)?;
    let fixtures = count_fixtures(&root)?;
    if fixtures != EXPECTED_FIXTURES {
        return Err(format!(
            "contract/fixtures holds {fixtures} fixtures, not {EXPECTED_FIXTURES}. The frozen \
             set is the contract; if it really changed, change EXPECTED_FIXTURES with it and \
             say why in the commit."
        ));
    }

    clear_workdir(&opts)?;

    build_target(&opts)?;
    prepare_weather_database(&opts)?;
    seed_weather_database(&opts)?;

    let mut procs = Processes::new(opts.keep_running);
    let config_db = bootstrap_configuration(&opts, &mut procs)?;
    seed_configuration(&opts, &config_db)?;
    start_daemon(&opts, &config_db, &mut procs)?;

    let runner = build_runner(&opts)?;

    // The accepted replay. `CONFORMANCE_FORCE_MANGLE=1` sends it through the proxy instead,
    // which is how `ci/conformance/README.md` says to watch this gate go red on demand. Now
    // that M4-2 has turned the direct replay green, it does change the verdict — and the
    // verdict it produces is deliberately **exit 2** rather than exit 1 below: the binary is
    // fine and the transport was corrupted on purpose, which is "this run proved nothing
    // about the contract" and not "the rewrite does not match".
    let forced = std::env::var_os("CONFORMANCE_FORCE_MANGLE").is_some_and(|v| v == "1");
    let mut direct_url = format!("http://127.0.0.1:{REST_PORT}");
    if forced {
        say("CONFORMANCE_FORCE_MANGLE=1: sending the accepted replay through mangle_proxy.py");
        start_proxy(&opts, &mut procs, "mangle-proxy-forced")?;
        direct_url = format!("http://127.0.0.1:{MANGLE_PORT}");
    }

    say(&format!("Replaying the fixtures against {direct_url}"));
    let direct = replay(&opts, &runner, &direct_url, "direct")?;
    println!("    {}", direct.summary_line());

    match judge_direct(&direct, EXPECTED_FIXTURES, &PENDING) {
        Direct::Inconclusive(why) => Ok(report(2, &why)),
        // A deliberately corrupted transport is not a contract verdict. Both halves of
        // CONFORMANCE_FORCE_MANGLE are exit 2 for the same reason: the knob exists to show
        // the instrument can move, and either outcome says something about the instrument
        // rather than about the binary.
        Direct::NotConforming(why) if forced => Ok(report(
            2,
            &format!(
                "CONFORMANCE_FORCE_MANGLE=1 sent the accepted replay through \
                 mangle_proxy.py, so this is the proxy biting and not a contract break. \
                 The suite can be made to fail, which is what the knob is for (W1 Decision \
                 10); the run proved nothing about the binary.\n  {why}"
            ),
        )),
        Direct::NotConforming(why) => {
            // A verdict, so `--keep-running` applies: a red replay is exactly when somebody
            // wants the daemon left up to curl it, and until M4-2 every replay is red. A
            // run that reached NO verdict is the one whose processes are a confusing
            // inheritance, and that is the branch above.
            procs.reached_a_verdict();
            Ok(report(1, &why))
        }
        Direct::Conforms if forced => {
            // The accepted replay went THROUGH the proxy and still conformed, so the proxy
            // mangled nothing. Caught here rather than by starting a second proxy on the
            // port the first one already holds, which is what the branch below would do.
            Ok(report(
                2,
                "the replay conformed while going through mangle_proxy.py, so the proxy \
                 changed nothing and CONFORMANCE_FORCE_MANGLE proved the opposite of what \
                 it is for (W1 Decision 10).",
            ))
        }
        Direct::Conforms => {
            // W1 Decision 10: a green conformance run is only accepted alongside a passing
            // negative control. Unreachable until M4-2, which is exactly why `judge_control`
            // is a pure function with its own tests — an assertion nobody has watched is
            // G-1, and "it cannot run yet" is not an exemption from that.
            say("The replay is clean, so W1 Decision 10 requires the negative control");
            start_proxy(&opts, &mut procs, "mangle-proxy")?;
            let control = replay(
                &opts,
                &runner,
                &format!("http://127.0.0.1:{MANGLE_PORT}"),
                "mangled",
            )?;
            println!("    {}", control.summary_line());
            match judge_control(
                &direct,
                &control,
                EXPECTED_FIXTURES,
                MIN_MANGLED_BROKEN,
                MIN_FROZEN_MANGLED_BROKEN,
            ) {
                Control::Held(note) => {
                    say(&format!(
                        "conformance: green. {} against the Rust binary, {note}",
                        direct.summary_line()
                    ));
                    procs.reached_a_verdict();
                    Ok(ExitCode::SUCCESS)
                }
                Control::Broken(why) => Ok(report(2, &why)),
            }
        }
    }
}

fn report(code: u8, why: &str) -> ExitCode {
    eprintln!("\nconformance: {why}");
    ExitCode::from(code)
}

// ---------------------------------------------------------------------------------------
// The verdict. Pure, and tested, because neither branch below can be reached from a green
// run until M4-2 exists.
// ---------------------------------------------------------------------------------------

/// What one replay reported, as read back out of the runner's own output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    /// Failures whose first line reads `request failed: …`, which is how
    /// `contract/conformance` reports a connection-level error — an ordinary `FAIL` line.
    pub transport: usize,
    /// One entry per fixture, read back out of the runner's per-fixture lines.
    ///
    /// The *policy* this gate applies is per class and per fixture name, so the counts are
    /// not enough: "26 failed" cannot tell a frozen-class break from the thirteen website
    /// fixtures nobody has implemented yet.
    pub fixtures: Vec<Fixture>,
}

/// One fixture's verdict, as the runner printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    /// The fixture's name.
    pub name: String,
    /// `frozen`, `coupled` or `retired`, from the `[...]` tag the runner prints.
    pub class: String,
    /// Whether it passed.
    pub passed: bool,
    /// Whether it was skipped, which is neither a pass nor a failure.
    pub skipped: bool,
    /// The deviation id the runner applied to it, if any — the second word of the tag.
    ///
    /// Read back so that [`judge_direct`] can assert *where* a deviation was applied and
    /// not merely that something declared it. Without this the gate cannot tell a deviated
    /// verdict from an ordinary one.
    pub deviation: Option<String>,
}

impl Summary {
    pub fn attempted(&self) -> usize {
        self.passed + self.failed + self.skipped
    }

    fn summary_line(&self) -> String {
        format!(
            "{} passed, {} failed, {} skipped",
            self.passed, self.failed, self.skipped
        )
    }

    /// Read the runner's report back.
    ///
    /// The *last* line matching the summary shape, not the first and not the final line of
    /// the file: stdout and stderr are merged into one log so that a failure is diagnosable
    /// from one place, and the order two streams arrive in is not a property worth
    /// depending on.
    pub fn parse(output: &str) -> Result<Summary, String> {
        let summary = output
            .lines()
            .rev()
            .find_map(parse_summary_line)
            .ok_or_else(|| {
                "the conformance runner printed no `N passed, M failed, K skipped` line, so \
                 it did not finish a replay"
                    .to_string()
            })?;
        Ok(Summary {
            // The runner's own indented failure detail, not merely the substring: a
            // fixture whose recorded body or description contained `request failed:` would
            // otherwise turn a verdict into exit 2. Fail-safe in the right direction, but
            // the prefix is free.
            transport: output
                .lines()
                .filter(|l| l.trim_start().starts_with("- request failed:"))
                .count(),
            fixtures: output.lines().filter_map(parse_fixture_line).collect(),
            ..summary
        })
    }

    /// The names of the fixtures in `class` that did not pass and were not skipped.
    ///
    /// A `SKIP` is "the fixture stores no body to compare against" and not "the fixture did
    /// not run", so it is neither -- the same distinction the counts make. It is also not a
    /// pass, which is why [`judge_direct`] refuses one in a gating class rather than
    /// reading this list and finding it empty.
    fn failed_in(&self, class: &str) -> Vec<&str> {
        self.fixtures
            .iter()
            .filter(|f| f.class == class && !f.passed && !f.skipped)
            .map(|f| f.name.as_str())
            .collect()
    }

    /// The names of the fixtures in `class` the runner SKIPPED.
    fn skipped_in(&self, class: &str) -> Vec<&str> {
        self.fixtures
            .iter()
            .filter(|f| f.class == class && f.skipped)
            .map(|f| f.name.as_str())
            .collect()
    }

    /// How many fixtures in `class` passed. The control's floor is measured per class, so
    /// "the proxy broke 20 responses" cannot be satisfied entirely by the website's.
    fn passed_in(&self, class: &str) -> usize {
        self.fixtures
            .iter()
            .filter(|f| f.class == class && f.passed)
            .count()
    }
}

/// `ok   <name> [<class>]`, `FAIL <name> [<class> <DEV-ID>]`, `SKIP <name> [<class>] (why)`.
///
/// The class tag is what work item N-18 adds to the runner's report, and reading it here is
/// what lets this gate's policy be per class without the runner knowing what a policy is.
/// A line that is not one of the three verdicts -- the indented failure detail, the
/// per-class summary -- yields `None`.
fn parse_fixture_line(line: &str) -> Option<Fixture> {
    let mut words = line.split_whitespace();
    let verdict = words.next()?;
    let (passed, skipped) = match verdict {
        "ok" => (true, false),
        "FAIL" => (false, false),
        "SKIP" => (false, true),
        _ => return None,
    };
    let name = words.next()?.to_string();
    // The tag, which must be present: a report without one is an older runner, and reading
    // its lines as unclassified would silently excuse every coupled-class failure.
    let class = words
        .next()?
        .strip_prefix('[')?
        .trim_end_matches(']')
        .to_string();
    // The tag's optional second word, which is a deviation id. Taken rather than discarded
    // because the gate asserts which fixtures a deviation was applied to, and a verdict
    // that does not say cannot be checked. Anything after it -- `SKIP`'s parenthesised
    // reason -- is not part of the tag.
    let deviation = words
        .next()
        .filter(|w| w.starts_with("DEV-"))
        .map(|w| w.trim_end_matches(']').to_string());
    Some(Fixture {
        name,
        class,
        passed,
        skipped,
        deviation,
    })
}

/// `N passed, M failed, K skipped`, and nothing else on the line.
fn parse_summary_line(line: &str) -> Option<Summary> {
    fn count<'a>(words: &mut impl Iterator<Item = &'a str>, label: &str) -> Option<usize> {
        let n = words.next()?.parse().ok()?;
        (words.next()? == label).then_some(n)
    }
    let mut words = line.trim().split(' ');
    let passed = count(&mut words, "passed,")?;
    let failed = count(&mut words, "failed,")?;
    let skipped = count(&mut words, "skipped")?;
    // Nothing after the counts, so that a sentence which merely contains them is not read
    // as a report.
    words.next().is_none().then_some(Summary {
        passed,
        failed,
        skipped,
        transport: 0,
        fixtures: Vec::new(),
    })
}

/// The accepted replay's verdict.
#[derive(Debug, PartialEq, Eq)]
pub enum Direct {
    /// Every fixture that ran passed. The negative control decides the gate from here.
    Conforms,
    /// The binary does not serve the frozen contract. Exit 1.
    NotConforming(String),
    /// Nothing was measured. Exit 2.
    Inconclusive(String),
}

/// Judge the accepted replay.
///
/// Order matters. "70 fixtures were attempted" and "the daemon was answering" are both
/// preconditions for the failure count meaning anything, so they are checked before it: a
/// run against a dead daemon reports 70 failures too, and reporting that as "the binary
/// does not conform" would be this gate lying in the more expensive direction.
///
/// Then the **policy**, which is per class and per fixture rather than per count -- see the
/// module documentation. The three parts in order of what they cost to get wrong:
///
/// 1. a `frozen`-class failure is a contract break, and `NotConforming`;
/// 2. a `coupled`-class failure that [`PENDING`] does not excuse is `NotConforming` too;
/// 3. a disagreement between [`PENDING`] and what actually failed is `Inconclusive`, in
///    **both** directions. An excused fixture nobody declared would mean this gate was
///    narrower than it says; a declared one that passed would mean the list is stale and the
///    gate is excusing something it no longer has to.
pub fn judge_direct(s: &Summary, expected: usize, pending: &[(&str, &str)]) -> Direct {
    if s.attempted() != expected {
        return Direct::Inconclusive(format!(
            "the runner attempted {} fixtures, expected {expected}.\n  \
             Something filtered the frozen set, so this run measured a subset and cannot \
             speak for the contract. CONFORMANCE_FIXTURES is the one knob that can do this; \
             otherwise contract/fixtures/ itself changed.",
            s.attempted()
        ));
    }
    if s.transport > 0 {
        return Direct::Inconclusive(format!(
            "{} of {} failures are transport failures (`request failed: …`), so the daemon \
             was not answering and this run proved nothing.\n  \
             contract/conformance reports a connection error as an ordinary FAIL line, which \
             is why this is checked rather than inferred from the count. The daemon's own log \
             is in the run's logs/ directory.",
            s.transport, s.failed
        ));
    }
    if s.passed == 0 && s.failed == 0 {
        return Direct::Inconclusive(format!(
            "all {} fixtures were SKIPPED, so nothing was compared.\n  \
             contract/conformance skips a fixture that stores no body, so a whole set of \
             them means the fixtures lost their recorded responses rather than that the \
             binary conforms.",
            s.skipped
        ));
    }
    // The classification has to have arrived, or everything below excuses nothing and
    // nothing. A report with no class tags is an older runner against a newer gate.
    if s.fixtures.len() != expected {
        return Direct::Inconclusive(format!(
            "the runner reported {} per-fixture verdicts for {expected} fixtures, so its \
             report could not be read.\n  \
             Every line must be `ok|FAIL|SKIP <name> [<class>]`; work item N-18 added the \
             class tag, and a report without it would make this gate excuse every \
             coupled-class failure.",
            s.fixtures.len()
        ));
    }

    // **The excuses below key on the name, so the names have to be unique.** `PENDING`'s
    // lookup is a `find`, and a fixture renamed onto a pending name would be excused by a
    // list that never meant it -- with the class totals, the `pending` fields and the count
    // all still exact.
    let mut seen: Vec<&str> = Vec::new();
    for f in &s.fixtures {
        if seen.contains(&f.name.as_str()) {
            return Direct::Inconclusive(format!(
                "`{}` was reported twice, so a name does not identify a fixture and the                  PENDING list cannot be read.\n                   Two files in contract/fixtures/ carry the same `name`; the unit test in                  this file pins a name to its filename for exactly this reason.",
                f.name
            ));
        }
        seen.push(f.name.as_str());
    }

    // **The class is data in the fixture, so the roster is checked here.** Without this the
    // gate's whole policy is one word in one JSON file: `latest-full-complement` moved to
    // `retired` stops gating, keeps the totals exact if something else moves the other way,
    // and goes green while a frozen `/latest` body contradicts the server.
    let mut misclassified: Vec<String> = Vec::new();
    for f in &s.fixtures {
        let declared = if FROZEN.contains(&f.name.as_str()) {
            "frozen"
        } else if RETIRED.contains(&f.name.as_str()) {
            "retired"
        } else {
            "coupled"
        };
        if f.class != declared {
            misclassified.push(format!(
                "`{}` is {} and this gate says {declared}",
                f.name, f.class
            ));
        }
    }
    if !misclassified.is_empty() {
        return Direct::Inconclusive(format!(
            "{} fixture(s) carry a class this gate does not declare: {}.\n               FROZEN and RETIRED in xtask/src/conformance.rs are the roster, and everything              else is coupled. Reclassifying a fixture changes whether it gates at all, so it              is a line in the gate rather than a word in a fixture -- and if the move is              right, this list is where it is recorded.",
            misclassified.len(),
            misclassified.join(", ")
        ));
    }

    // **A deviation is "compare against bytes the author wrote", so where it was applied is
    // the whole of its safety.** `main.rs` refuses an id no fixture declares, which is the
    // typo direction; this is the other one -- a `deviation` block added to a fixture the
    // owner never signed off on would print `ok` and the gate would read it as an ordinary
    // pass, because the id is in the tag and nothing compared it to anything.
    for (id, fixtures) in DEVIATIONS {
        let applied: Vec<&str> = s
            .fixtures
            .iter()
            .filter(|f| f.deviation.as_deref() == Some(id))
            .map(|f| f.name.as_str())
            .collect();
        let undeclared: Vec<&str> = applied
            .iter()
            .copied()
            .filter(|n| !fixtures.contains(n))
            .collect();
        if !undeclared.is_empty() {
            return Direct::Inconclusive(format!(
                "`{id}` was applied to {} fixture(s) this gate does not declare it for: {}.\n                   A deviated fixture is measured against a response its author wrote instead                  of the bytes the Go server produced, so the fixture list in DEVIATIONS is                  the signature on it (docs/reconciliation.md §3). Add it there, with the                  reason, or remove the deviation block.",
                undeclared.len(),
                undeclared.join(", ")
            ));
        }
        let missing: Vec<&str> = fixtures
            .iter()
            .copied()
            .filter(|n| !applied.contains(n))
            .collect();
        if !missing.is_empty() {
            return Direct::Inconclusive(format!(
                "`{id}` is declared for {} fixture(s) the runner did not apply it to: {}.\n                   Either the fixture lost its deviation block -- in which case it is now                  being measured against the Go bytes and the entry is stale -- or it was                  renamed. Both make this gate's record of what shipped wrong.",
                missing.len(),
                missing.join(", ")
            ));
        }
    }

    // **A SKIP in a gating class is not a pass.** `contract/conformance` skips a fixture
    // that stores no body, and the counts put it in neither bucket -- so without this a
    // `frozen` fixture whose recorded body went missing goes green with the 133-key DTO,
    // both duplicate pairs and every `omitempty` decision uncompared. The all-skipped check
    // above only catches losing every body at once, which is the easy case.
    //
    // `retired` may skip: nothing gates on it either way.
    for class in ["frozen", "coupled"] {
        let skipped = s.skipped_in(class);
        if !skipped.is_empty() {
            return Direct::Inconclusive(format!(
                "{} {class}-class fixture(s) were SKIPPED, so their bodies were never                  compared: {}.\n                   A skip means the fixture stores no body to compare against. The status line                  and the headers still matched, which is why this is not a failure -- and                  why it must not be read as a pass either. Re-capture them with                  contract/tools/capture.py.",
                skipped.len(),
                skipped.join(", ")
            ));
        }
    }

    let frozen = s.failed_in("frozen");
    if !frozen.is_empty() {
        return Direct::NotConforming(format!(
            "{} FROZEN-class fixture(s) failed, which is a contract break: {}.\n  \
             These are the five endpoints the iOS client calls (D-68), and that client is \
             deployed independently of this server -- so a diff here is every phone until \
             its users update, not a refactor. The failures are named in the run's \
             direct.txt.",
            frozen.len(),
            frozen.join(", ")
        ));
    }

    let coupled = s.failed_in("coupled");
    let unexcused: Vec<&str> = coupled
        .iter()
        .copied()
        .filter(|name| !pending.iter().any(|(excused, _)| excused == name))
        .collect();
    if !unexcused.is_empty() {
        return Direct::NotConforming(format!(
            "{} COUPLED-class fixture(s) failed and nothing excuses them: {}.\n  \
             Coupled is not unconstrained (docs/reconciliation.md §6.4): the website's \
             endpoints may change, but only in lockstep with the embedded JavaScript in the \
             same PR -- so either the server changed without its client or the fixture needs \
             re-capturing in that same PR.",
            unexcused.len(),
            unexcused.join(", ")
        ));
    }

    // Both directions, because a stale excuse is as bad as an undeclared one.
    let mut stale: Vec<&str> = Vec::new();
    for (name, owner) in pending {
        let verdict = s.fixtures.iter().find(|f| f.name == *name);
        match verdict {
            None => {
                return Direct::Inconclusive(format!(
                    "`{name}` is declared pending on {owner} and the runner did not report \
                     it at all, so this gate excused a fixture that was not replayed. \
                     Either the fixture was renamed or the PENDING list in \
                     xtask/src/conformance.rs is out of date."
                ))
            }
            Some(f) if f.passed => stale.push(*name),
            Some(_) => {}
        }
    }
    if !stale.is_empty() {
        return Direct::Inconclusive(format!(
            "{} fixture(s) are declared pending and PASSED: {}.\n  \
             The work item that implements them has landed, so the entry in \
             xtask/src/conformance.rs's PENDING list is excusing a failure that cannot \
             happen -- remove it, which is what makes this gate narrow again on purpose \
             rather than by accident.",
            stale.len(),
            stale.join(", ")
        ));
    }

    Direct::Conforms
}

/// The negative control's verdict (W1 Decision 10).
#[derive(Debug, PartialEq, Eq)]
pub enum Control {
    /// The control bit, in all three of the ways it has to.
    Held(String),
    /// It did not, so the accepted replay above cannot be trusted. Exit 2.
    Broken(String),
}

/// Judge the negative control (W1 Decision 10).
///
/// "It failed" is not the assertion. The assertion is that the proxy **broke responses the
/// target was serving**: it only rewrites JSON bodies, so the fixtures it does not touch
/// must still pass, no failure may be a transport failure, and the number it broke is
/// measured against the direct replay rather than against a failure count. Without the last
/// part a daemon that died between the replays fails all 70, clears a bare floor, and
/// reports a control that held while having mangled nothing -- and against this target a
/// proxy that mangled nothing clears a failure floor of 22 on its own.
pub fn judge_control(
    direct: &Summary,
    s: &Summary,
    expected: usize,
    min_broken: usize,
    min_frozen_broken: usize,
) -> Control {
    // The same precondition the direct replay has. It holds by construction today -- one
    // runner, one fixture set, two base URLs -- and asserting it here costs nothing and
    // stops the control from being judged on a subset if that ever stops being true.
    if s.attempted() != expected {
        return Control::Broken(format!(
            "the negative control attempted {} fixtures, expected {expected}.",
            s.attempted()
        ));
    }
    if s.failed == 0 {
        return Control::Broken(
            "the negative control PASSED. The runner is not asserting anything: a conformance \
             suite that cannot be made to fail proves nothing (W1 Decision 10)."
                .to_string(),
        );
    }
    if s.transport > 0 {
        return Control::Broken(format!(
            "the negative control saw {} transport failures rather than contract failures: the \
             daemon or the proxy was not answering, so this run proved nothing.",
            s.transport
        ));
    }
    if s.passed == 0 {
        return Control::Broken(
            "every fixture failed through the proxy, including the ones it does not mangle. \
             That is a broken forward path, not a working negative control."
                .to_string(),
        );
    }
    if s.passed > direct.passed {
        return Control::Broken(format!(
            "{} fixtures passed through the proxy and only {} passed directly, so the proxy \
             FIXED responses rather than breaking them. Something other than the proxy \
             changed between the two replays.",
            s.passed, direct.passed
        ));
    }
    let broken = direct.passed - s.passed;
    if broken < min_broken {
        return Control::Broken(format!(
            "the negative control broke only {broken} of the {} responses the target serves, \
             expected at least {min_broken}.\n  \
             Something stopped asserting. This is a count of responses the proxy BROKE and \
             not of fixtures that failed, because {} fixtures fail whatever the proxy does \
             -- nine are retired and thirteen are not implemented yet -- so a floor on the \
             failure count would be cleared by a proxy that mangled nothing.",
            direct.passed, direct.failed
        ));
    }
    // **And a floor on the FROZEN half.** The total floor is strong today only by accident:
    // six coupled fixtures pass directly, so most of the 20 must be frozen. Once M4-3 lands
    // all 19 coupled pass, and 19 coupled + 1 frozen would clear a total of 20 while the
    // control proved almost nothing about the five endpoints the iOS client calls. The
    // floor that matters is therefore per class, and it has to be stated now rather than
    // discovered when the total stops binding.
    let frozen_broken = direct
        .passed_in("frozen")
        .saturating_sub(s.passed_in("frozen"));
    if frozen_broken < min_frozen_broken {
        return Control::Broken(format!(
            "the negative control broke only {frozen_broken} of the {} FROZEN-class \
             responses the target serves, expected at least {min_frozen_broken}.\n  \
             {broken} responses were broken in total, which clears the overall floor -- but \
             the five frozen endpoints are the ones deployed clients call, so a control that \
             only breaks the website's responses is not asserting the thing this suite \
             exists for.",
            direct.passed_in("frozen")
        ));
    }
    Control::Held(format!(
        "{broken} of the {} responses the target serves were broken by the proxy \
         ({frozen_broken} of {} frozen) and {} fixtures still passed",
        direct.passed,
        direct.passed_in("frozen"),
        s.passed
    ))
}

// ---------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------

#[derive(Debug)]
struct Options {
    root: PathBuf,
    /// The binary to replay against.
    target_binary: PathBuf,
    /// Whether to build it first. The plan's first verb is "build the binary", so this is
    /// on by default; `--no-build` is for a caller that has one already.
    build: bool,
    /// `--release`, so that `--target-binary target/release/modernweather` is reachable
    /// without a second command.
    release: bool,
    workdir: PathBuf,
    log_dir: PathBuf,
    keep_running: bool,
    db: Libpq,
}

/// The flags that take a value, and the ones that do not.
const VALUE_FLAGS: [&str; 2] = ["--target-binary", "--workdir"];
const BARE_FLAGS: [&str; 3] = ["--no-build", "--release", "--keep-running"];

impl Options {
    /// `Ok(None)` means "parsed, and the caller asked for help instead of a run".
    fn parse(root: &Path, args: &[String]) -> Result<Option<Options>, String> {
        for a in args {
            if matches!(a.as_str(), "-h" | "--help") {
                crate::usage();
                return Ok(None);
            }
            if !a.starts_with('-') {
                continue;
            }
            let head = a.split('=').next().unwrap_or(a);
            // `--no-build=x` used to pass this check and then match nothing, so the flag was
            // silently ignored: a bare flag spelled with a value is a typo, not a default.
            if BARE_FLAGS.contains(&head) && head != a.as_str() {
                return Err(format!("`{head}` takes no value, so `{a}` is a typo."));
            }
            if !VALUE_FLAGS.contains(&head) && !BARE_FLAGS.contains(&head) {
                return Err(format!(
                    "unknown option `{a}`. Run `cargo xtask help` for the accepted ones."
                ));
            }
        }
        let release = args.iter().any(|a| a == "--release");
        let profile_dir = if release { "release" } else { "debug" };
        let target_binary = value_of(args, "--target-binary")?
            .unwrap_or_else(|| format!("target/{profile_dir}/modernweather"));
        let workdir =
            value_of(args, "--workdir")?.unwrap_or_else(|| "target/conformance-rust".to_string());
        // `Path::join` REPLACES the base when the argument is absolute, and `clear_workdir`
        // deletes inside whatever comes out -- so `--workdir /var` would have it removing
        // /var/logs. Confined to the repository rather than sanitised, because every
        // legitimate value is already relative and there is nothing this command needs to
        // write outside the tree it was invoked in.
        let workdir = confine(root, &workdir, "--workdir")?;
        Ok(Some(Options {
            root: root.to_path_buf(),
            target_binary: root.join(target_binary),
            build: !args.iter().any(|a| a == "--no-build"),
            release,
            log_dir: workdir.join("logs"),
            workdir,
            keep_running: args.iter().any(|a| a == "--keep-running"),
            db: Libpq::from_env(),
        }))
    }
}

/// A flag's value, distinguishing "absent" from "present with nothing after it".
///
/// `crate::flag_value` collapses the two into `None`, which for a flag that decides which
/// binary gets measured would mean a typo quietly measuring the default one instead.
fn value_of(args: &[String], name: &str) -> Result<Option<String>, String> {
    let eq = format!("{name}=");
    let present = args.iter().any(|a| a == name || a.starts_with(&eq));
    match crate::flag_value(args, name) {
        None if present => Err(format!("`{name}` needs a value")),
        Some(v) if v.trim().is_empty() => Err(format!("`{name}` was given an empty value")),
        other => Ok(other),
    }
}

/// `root/candidate`, rejected unless it really is under `root`.
fn confine(root: &Path, candidate: &str, flag: &str) -> Result<PathBuf, String> {
    let joined = root.join(candidate);
    // Lexical, because the path does not exist yet and `canonicalize` therefore cannot be
    // asked. `..` is rejected outright rather than resolved: there is no legitimate value
    // containing one, so refusing is both simpler and impossible to get subtly wrong.
    if Path::new(candidate).is_absolute() {
        return Err(format!(
            "{flag} must be relative to the repository root; `{candidate}` is absolute, and \
             this command deletes inside the directory it is given."
        ));
    }
    if candidate.split('/').any(|part| part == "..") {
        return Err(format!(
            "{flag} must stay inside the repository root; `{candidate}` escapes it, and this \
             command deletes inside the directory it is given."
        ));
    }
    Ok(joined)
}

/// The weather database, addressed the way every other script in this repository addresses
/// one: libpq's own variables, so that CI's service container and a developer's local
/// server are configured the same way and neither is special-cased here.
#[derive(Debug)]
struct Libpq {
    host: String,
    port: String,
    user: String,
    password: String,
    database: String,
    sslmode: String,
}

impl Libpq {
    fn from_env() -> Libpq {
        let var = |name: &str, default: &str| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Libpq {
            host: var("PGHOST", "127.0.0.1"),
            port: var("PGPORT", "5432"),
            user: var("PGUSER", "postgres"),
            // Empty is a legitimate value (`POSTGRES_HOST_AUTH_METHOD: trust`), so this one
            // is not defaulted through `filter(non-empty)` like the others.
            password: std::env::var("PGPASSWORD").unwrap_or_default(),
            // Deliberately not `modernweather`: the database below is dropped, and a
            // developer's local database is exactly what that name would be.
            database: var("PGDATABASE", "mw_conformance"),
            sslmode: var("PGSSLMODE", "disable"),
        }
    }

    /// The URL **without** the password, which is the one that goes on a command line.
    ///
    /// `psql` is handed the password through `PGPASSWORD` in its environment instead
    /// (see [`psql`]), so it never reaches `argv` and therefore never reaches `/proc` or an
    /// error message. The user does have to be in the URL: this command's default user is
    /// `postgres`, while `psql`'s own default is the operating-system user, so leaving it
    /// out would silently connect as somebody else.
    fn url(&self, database: &str) -> String {
        format!(
            "postgresql://{}@{}:{}/{database}?sslmode={}",
            encode(&self.user),
            self.host,
            self.port,
            self.sslmode
        )
    }

    /// What goes into `weather_database.url`: the full thing, because sqlx is given no
    /// environment of its own.
    ///
    /// The column's CHECK pins the scheme to `postgres://` or `postgresql://`, which this
    /// satisfies, and `mw_store::version` hands the whole string to sqlx. The credentials
    /// are percent-encoded, which is not cosmetic: an unencoded `p@ss/word` turns
    /// `postgresql://postgres:p@ss/word@127.0.0.1:5432/db` into a URL whose host parses as
    /// `ss`, and the daemon would then warn and start anyway under **D-27** -- caught by the
    /// `database version asserted` check in `start_daemon`, but as a confusing failure
    /// rather than as this one.
    fn target_url(&self) -> String {
        if self.password.is_empty() {
            return self.url(&self.database);
        }
        format!(
            "postgresql://{}:{}@{}:{}/{}?sslmode={}",
            encode(&self.user),
            encode(&self.password),
            self.host,
            self.port,
            self.database,
            self.sslmode
        )
    }
}

/// Percent-encode a URL userinfo component: unreserved characters through, everything else
/// escaped. Hand-rolled because `xtask` has three dependencies and this is nine lines
/// (**D-32**: the crate list is a decision, not a convenience).
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------------------

/// Named up front rather than discovered three minutes in.
///
/// `psql` is the only way this run reaches the weather schema and `sqlite3` the only way it
/// writes the configuration, exactly as in `go-target.sh`; `python3` runs the negative
/// control. All three are checked even when the run will not reach the control, because a
/// tool missing at the end of a long run is the same defect discovered later.
fn preflight(opts: &Options) -> Result<(), String> {
    let mut required = vec!["psql", "sqlite3", "python3"];
    if opts.build || std::env::var_os("CONFORMANCE_BIN").is_none() {
        required.push("cargo");
    }
    let missing: Vec<&str> = required
        .into_iter()
        .filter(|t| which(t).is_none())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing required tool(s): {}.\n  \
             This gate stands a real daemon up against a real TimescaleDB; it fails rather \
             than skipping, because a conformance check that reports green without running \
             is docs/reconciliation.md G-1.",
            missing.join(", ")
        ));
    }

    // Every port this run binds, before anything is started. A daemon or proxy leaked from
    // an earlier run satisfies the readiness probe below while the freshly started one dies
    // on "address already in use" — so the replay would silently measure the stale process.
    //
    // Before the build rather than at the statement itself, so a `PGDATABASE` this run will
    // not touch is refused in a second instead of after three minutes of `cargo build`.
    droppable(&opts.db.database)?;

    // The schema's 8080 is deliberately NOT in this list: the bootstrap start binds it only
    // because the row that says otherwise is the one it is creating, and that start's own
    // fate is not used for anything (see `bootstrap_configuration`).
    ports_free(&[REST_PORT, MANGLE_PORT])
}

/// The one guard on a `DROP DATABASE ... WITH (FORCE)` whose name comes from the
/// environment.
///
/// The default is safe; `PGDATABASE` exported to a development weather database is not, and
/// "the README says not to point it at anything you care about" is not a mechanism.
/// `conformance` in the name is the convention both halves of this suite already follow
/// (`go-target.sh` defaults to `conformance`, this to `mw_conformance`), so the rule costs a
/// correctly-named database nothing.
///
/// Called from `preflight` as well as from the statement itself: a misconfigured
/// `PGDATABASE` should be refused before a three-minute build, and the invariant should also
/// hold at the point where it matters.
fn droppable(database: &str) -> Result<(), String> {
    if database.contains("conformance") {
        return Ok(());
    }
    Err(format!(
        "refusing to drop `{database}`: this command recreates $PGDATABASE from scratch, so \
         it only accepts a name containing `conformance`.\n  \
         Unset PGDATABASE to use the default (`mw_conformance`), or point it at a database \
         whose name says what it is for."
    ))
}

/// Refuse to start if anything is already answering on a port this run must own.
///
/// Separate from [`preflight`] so that a test can hand it a port the test itself bound,
/// rather than one a concurrent run of this command might be using.
fn ports_free(ports: &[u16]) -> Result<(), String> {
    for port in ports {
        if port_answers(*port) {
            return Err(format!(
                "port {port} is already in use. A daemon or proxy leaked from an earlier run \
                 would be measured instead of the one this command starts. Find it with \
                 `ss -ltnp | grep {port}` and kill it."
            ));
        }
    }
    Ok(())
}

/// Everything a previous run left in the work directory, removed by name.
///
/// By name rather than by removing the directory, which matters in CI more than it looks:
/// `Swatinem/rust-cache` restores `target/` wholesale, so the default work directory arrives
/// pre-populated with the previous run's replay output and logs. `ci/conformance/go-target.sh`
/// solved the same problem by moving its workdir out of `target/`, which is not available
/// here -- `ci/gates.toml` fixes this command's arguments. A stale `mangled.txt` uploaded as
/// this run's artefact is a small lie, and small lies about what a conformance run measured
/// are this gate's whole subject.
fn clear_workdir(opts: &Options) -> Result<(), String> {
    for name in [
        "config.db",
        "config.db-wal",
        "config.db-shm",
        "direct.txt",
        "mangled.txt",
    ] {
        let p = opts.workdir.join(name);
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        }
    }
    if opts.log_dir.exists() {
        std::fs::remove_dir_all(&opts.log_dir)
            .map_err(|e| format!("{}: {e}", opts.log_dir.display()))?;
    }
    std::fs::create_dir_all(&opts.log_dir).map_err(|e| format!("{}: {e}", opts.log_dir.display()))
}

fn count_fixtures(root: &Path) -> Result<usize, String> {
    let dir = root.join("contract/fixtures");
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    Ok(entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .count())
}

// ---------------------------------------------------------------------------------------
// Step 2: the binary, and the weather database
// ---------------------------------------------------------------------------------------

fn build_target(opts: &Options) -> Result<(), String> {
    if opts.build {
        say("Building the daemon");
        let mut cmd = cargo(&opts.root);
        cmd.args(["build", "--locked", "--bin", "modernweather"]);
        if opts.release {
            cmd.arg("--release");
        }
        run(cmd, "cargo build --bin modernweather")?;
    }
    if !opts.target_binary.is_file() {
        return Err(format!(
            "{} does not exist.{}",
            opts.target_binary.display(),
            if opts.build {
                " The build above reported success, so --target-binary names the wrong path \
                 (a --release build writes target/release/)."
            } else {
                " --no-build was given, so this command did not build it."
            }
        ));
    }
    Ok(())
}

/// Recreate the weather database and apply `migrations/`.
///
/// A fresh database rather than a truncate, for the same reason `go-target.sh` gives: the
/// schema is provisioned once over an empty database, and re-running this command must not
/// depend on what the last run left behind.
///
/// `ci/sql/assert-timescaledb-version.sql` runs first. The daemon asserts the same thing at
/// startup (**D-62**) and would refuse to boot, so this is not the assertion — it is the
/// *message*: a run that fails here says "PostgreSQL 15 is older than the required 17"
/// instead of making the reader work that out from a daemon that exited during step 4a.
fn prepare_weather_database(opts: &Options) -> Result<(), String> {
    let db = &opts.db;
    // The one guard on a `DROP DATABASE ... WITH (FORCE)` whose name comes from the
    // environment. The default is safe; `PGDATABASE` exported to a development weather
    // database is not, and "the README says not to point it at anything you care about" is
    // not a mechanism. `conformance` in the name is the convention both halves of this
    // suite already follow (`go-target.sh` defaults to `conformance`, this to
    // `mw_conformance`), so the rule costs a correctly-named database nothing.
    droppable(&db.database)?;
    say(&format!(
        "Recreating the weather database {} on {}:{}",
        db.database, db.host, db.port
    ));
    let admin = db.url("postgres");
    await_for("PostgreSQL", Duration::from_secs(120), || {
        if psql(opts, &admin, &["-c", "select 1"]).is_ok() {
            Wait::Ready
        } else {
            Wait::Pending
        }
    })
    .map_err(|e| {
        format!(
            "{e}\n  Set PGHOST/PGPORT/PGUSER/PGPASSWORD/PGDATABASE/PGSSLMODE to reach one. \
             CI runs the timescale/timescaledb-ha:pg17 image (DESIGN.md D-2)."
        )
    })?;

    psql(
        opts,
        &admin,
        &[
            "-c",
            &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", db.database),
        ],
    )?;
    psql(
        opts,
        &admin,
        &["-c", &format!("CREATE DATABASE {}", db.database)],
    )?;

    let target = db.target_url();
    // The daemon does not create its own extensions; **D-8** forbids startup DDL and the
    // migrations assume `timescaledb` is already there, the same way `migrate-twice.sh`
    // does. `timescaledb_toolkit` is created when the server has it and skipped when it has
    // not, which is exactly what the version assertion treats as a warning rather than an
    // error (W4 owns OQ-3).
    psql(
        opts,
        &target,
        &["-c", "CREATE EXTENSION IF NOT EXISTS timescaledb"],
    )?;
    let _ = psql(
        opts,
        &target,
        &["-c", "CREATE EXTENSION IF NOT EXISTS timescaledb_toolkit"],
    );
    let said = psql(
        opts,
        &target,
        &["-f", "ci/sql/assert-timescaledb-version.sql"],
    )?;
    for line in said.lines() {
        println!("    {line}");
    }

    say("Applying migrations/");
    for path in sql_files(&opts.root.join("migrations"))? {
        println!("    {}", path.display());
        // `-1` wraps each migration in one transaction, which is the property of
        // `sqlx::migrate` that matters here: a migration that fails half way through leaves
        // nothing behind. Applied in filename order, which is the other one.
        psql(opts, &target, &["-1", "-f", &path.to_string_lossy()])?;
    }
    Ok(())
}

/// Apply `ci/conformance/rust-seed/`'s weather half.
///
/// **This is what makes a green replay mean anything.** M2-5 brought the daemon up against
/// a freshly migrated but *empty* database, which is all a replay against 404s needs; every
/// frozen endpoint reads, so a run without this would report a contract failure for every
/// fixture that has data in it. `ci/conformance/README.md` records it as this item's.
///
/// A **translation** of W1's rig (`contract/tools/rig/`) rather than a copy: that rig writes
/// the Go `weather` table and the Go continuous aggregates, and these files write
/// `observations` and refresh W4's four levels.
///
/// `01-config.sql` is skipped here -- it is the SQLite half and belongs to
/// [`seed_configuration`], which runs between the two daemon starts.
fn seed_weather_database(opts: &Options) -> Result<(), String> {
    say("Seeding the weather database (W1's rig, translated to the new schema)");
    let target = opts.db.target_url();
    for path in sql_files(&opts.root.join(WEATHER_SEED))? {
        if path.file_name().is_some_and(|n| n == "01-config.sql") {
            continue;
        }
        println!("    {}", path.display());
        // **Not `-1`**, unlike the migrations: `03-refresh.sql` calls
        // `refresh_continuous_aggregate`, which is a procedure that manages its own
        // transactions and cannot run inside one. Each file opens its own where it wants
        // one, which is why the two that write rows begin with `BEGIN`.
        psql(opts, &target, &["-f", &path.to_string_lossy()])?;
    }
    // Asserted rather than assumed: a seed that silently inserted nothing -- a typo in a
    // column name is caught by `ON_ERROR_STOP`, but a `WHERE` or a `generate_series` that
    // excluded everything is not -- would make every data-bearing fixture fail on an empty
    // body and read as a contract failure.
    // **Every relation a frozen endpoint reads, not just the first two.** The two that were
    // asserted here are the ones whose absence a fixture would notice loudly; the others are
    // the dangerous ones, because an empty table and a correct answer are the same bytes:
    //
    //   - `snow_totals_cache` -- all five `/snow` fixtures carry the same eight keys, and
    //     `snow-device-never-reported` is specifically the all-zeros no-row case. The
    //     comparator checks shape and numeric convention, and `0` satisfies both, so an
    //     empty cache leaves four FROZEN fixtures green against a `/snow` serving zeros for
    //     every station.
    //   - `observations_1h` / `observations_1d` -- `03-refresh.sql`'s order is load-bearing
    //     because each level reads the one below. A level that materialised nothing leaves
    //     `/span/72h` and `/span/720h` answering with D-55's trailing raw bucket alone: one
    //     element, of the right shape, which passes.
    //   - `almanac_cache` and `forecasts` are caught today -- `{}` and a 404 differ in shape
    //     -- and are asserted anyway, because that is a property of the fixtures rather than
    //     of the seed.
    let counts = [
        ("observations", "readings"),
        ("observations_5m", "five-minute buckets"),
        ("observations_1h", "hourly buckets"),
        ("observations_1d", "daily buckets"),
        ("almanac_cache", "almanac rows"),
        ("snow_totals_cache", "snow rows"),
        ("forecasts", "forecast rows"),
    ];
    let mut empty: Vec<&str> = Vec::new();
    for (relation, label) in counts {
        let n = psql_query(opts, &target, &format!("select count(*) from {relation}"))?;
        println!("    {n} {label}");
        if n == "0" {
            empty.push(relation);
        }
    }
    if !empty.is_empty() {
        return Err(format!(
            "the seed left {} empty, so this run would measure a daemon with no data for \
             the endpoints that read {}.\n  \
             Every frozen endpoint reads something, and for `/snow` an empty cache and a \
             correct answer are the same bytes -- the fixtures assert shape and numeric \
             convention, and all-zeros satisfies both. Check \
             ci/conformance/rust-seed/02-weather.sql, 03-refresh.sql and 04-caches.sql.",
            empty.join(", "),
            if empty.len() == 1 { "it" } else { "them" }
        ));
    }

    // **Per station, and in both directions.** A total is not enough, and that is not
    // theoretical: the first version of `02-weather.sql` inserted nothing at all for
    // `csi-minimal` -- a `% 300 = 0` filter over a series stepping from `now()`, which is
    // never on a minute boundary -- and the total was still five figures. Two fixtures read
    // that station and would have failed on the contract rather than on the seed.
    //
    // The two stations with NO readings are asserted as well, because they are what three
    // fixtures are: `/latest` 404s, `/span` is `[]`, `/almanac` is `{}`.
    for (station, expect_data) in SEEDED_STATIONS {
        let n = psql_query(
            opts,
            &target,
            &format!("select count(*) from observations where station_id = '{station}'"),
        )?;
        let has_data = n != "0";
        if has_data != expect_data {
            return Err(format!(
                "`{station}` has {n} reading(s) and the rig requires it to have {}.\n  \
                 ci/conformance/rust-seed/02-weather.sql names which fixtures each station \
                 serves; a station seeded when it should be empty makes `/latest` a 200 \
                 where the contract says 404, and one left empty makes every fixture that \
                 reads it fail on the contract rather than on the seed.",
                if expect_data { "some" } else { "none" }
            ));
        }
        println!("    {station}: {n} reading(s)");
    }
    Ok(())
}

/// Every `*.sql` directly under `dir`, in filename order.
///
/// Not recursive, and that is load-bearing in both directories this is used on:
/// `migrations/config/` is a **second database** on SQLite (**D-8** as amended by A-W6-2),
/// and handing its DDL to `psql` is what a recursive walk would do.
fn sql_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("reading {}: {e}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "sql"))
        .collect();
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Step 3: the configuration database
// ---------------------------------------------------------------------------------------

/// Create the configuration database by letting the daemon create it.
///
/// There is no bootstrap-only mode to invoke: `open_and_migrate` is step 3 of the boot
/// sequence and the process goes on to run the application. So the daemon is started, waited
/// on until `_sqlx_migrations` holds one successful row per file under `migrations/config/`,
/// and stopped.
///
/// **Why not apply those files with `sqlite3` instead.** The daemon applies them with
/// `sqlx::migrate!`, which records a checksum per migration and refuses to run against a
/// schema it did not create. A second applier would therefore have to forge
/// `_sqlx_migrations` rows, and a forged ledger that drifts from sqlx's own format fails at
/// the one moment this gate is being trusted. Letting the real migrator run is also the
/// stronger check: the configuration database this replay uses is the one the shipped binary
/// produces.
///
/// **What this start binds.** `migrations/config/` seeds `listener` with `0.0.0.0:8080`, so
/// that is where this one serves `/metrics` for the second or two it lives. Its fate is not
/// used: the loop below waits for the *schema*, and step 3 of the boot sequence commits the
/// migrations before step 6a binds anything — so an 8080 already in use makes this process
/// exit after having done the only job it was started for. That ordering is asserted rather
/// than assumed: a process that dies before the schema appears fails the run and names its
/// log.
fn bootstrap_configuration(opts: &Options, procs: &mut Processes) -> Result<PathBuf, String> {
    // Removed by `clear_workdir` rather than here, so that one place decides what a fresh
    // run starts from.
    let config_db = opts.workdir.join("config.db");
    let migrations = sql_files(&opts.root.join("migrations/config"))?.len();
    say(&format!(
        "Bootstrapping the configuration database ({migrations} migration(s), port {SCHEMA_PORT} \
         while it lives)"
    ));
    let mut cmd = Command::new(&opts.target_binary);
    cmd.args(["--config"])
        .arg(&config_db)
        .args(["--log-format", "json"]);
    let bootstrap = procs.spawn(opts, "bootstrap", cmd)?;

    let applied = || -> bool {
        sqlite_query(
            &config_db,
            "select count(*) from _sqlx_migrations where success <> 0",
        )
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
            == Some(migrations)
    };
    let log = opts.log_dir.join("bootstrap.log");
    await_for("the configuration schema", Duration::from_secs(60), || {
        if applied() {
            return Wait::Ready;
        }
        // A process that has exited will not apply anything else, so this stops waiting
        // rather than spending the deadline -- and it asks `applied` a second time
        // before calling the exit a failure. The commit and the exit can land inside one
        // poll interval, which is exactly what happens in the case the doc comment above
        // calls harmless: the migrations commit, the bind then fails, and the process is
        // gone by the time this looks.
        if procs.exited(bootstrap) {
            if applied() {
                return Wait::Ready;
            }
            return Wait::GiveUp(format!(
                "the bootstrap daemon exited before it had applied {migrations} \
                     configuration migration(s)"
            ));
        }
        Wait::Pending
    })
    .map_err(|e| format!("{e}. The daemon's log is {}", log.display()))?;

    // Stopped before anything writes to the file. `rw`'s equivalent step in `go-target.sh`
    // raced the seed and failed on `database is locked (5)`; SIGKILL and a `wait` is the
    // blunter version of the same fix, and it is safe here precisely because the migrations
    // have committed — this process has nothing left to flush.
    procs.stop(bootstrap)?;
    Ok(config_db)
}

/// The two rows that make this a seeded configuration rather than a default one.
///
/// `listener` is seeded by the migration and *updated* here; `weather_database` has no
/// seeded row at all, which is **D-44**'s "a node with no row has no `db:postgres`" — so
/// writing it is what connects the daemon to the database prepared above.
fn seed_configuration(opts: &Options, config_db: &Path) -> Result<(), String> {
    say("Seeding the listener and the weather database");
    sqlite_write(
        config_db,
        &format!(
            // 127.0.0.1 rather than 0.0.0.0: this run talks to itself, and a gate that
            // publishes a half-built daemon on every interface is a gate with a side effect.
            "UPDATE listener SET listen_addr = '127.0.0.1', http_port = {REST_PORT} WHERE id = 1;"
        ),
    )?;
    sqlite_write(
        config_db,
        &format!(
            "INSERT INTO weather_database (id, url) VALUES (1, '{}');",
            // Doubling is the whole of SQL string escaping here, and the value is a URL
            // assembled from the environment rather than from a fixture.
            opts.db.target_url().replace('\'', "''")
        ),
    )?;
    // W1's seven stations and six websites, in the new schema. Here rather than in
    // `seed_weather_database` because it is a different database and a different tool, and
    // because it has to run while the daemon is **stopped** -- the bootstrap start holds
    // the file open until it exits, and seeding into that window is what failed
    // intermittently with `database is locked (5)` in `go-target.sh`.
    let path = opts.root.join(CONFIG_SEED);
    let sql = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("    {}", path.display());
    sqlite_write(config_db, &sql)?;
    let stations = sqlite_query(config_db, "select count(*) from stations")?;
    let websites = sqlite_query(config_db, "select count(*) from website")?;
    println!(
        "    {} station(s) and {} website(s)",
        stations.trim(),
        websites.trim()
    );
    // The counts W1 chose, asserted: seven devices and six websites is what every one of
    // the 70 fixtures was captured against, and a seed that applied partially would make
    // the failures read as contract failures.
    if stations.trim() != "7" || websites.trim() != "6" {
        return Err(format!(
            "the configuration seed produced {} station(s) and {} website(s), expected 7 \
             and 6.\n  \
             W1 chose exactly those so that every branch of the frozen contract is \
             reachable (ci/conformance/config-seed.sql), and three fixtures address a \
             station by its id -- so a partial seed measures a different configuration \
             from the one the fixtures describe.",
            stations.trim(),
            websites.trim()
        ));
    }
    Ok(())
}

/// Start the daemon for real, and prove it is the one answering.
///
/// Three facts are established before a fixture is replayed, because each of them is a way
/// this gate could otherwise report a conformance result it had not measured:
///
/// 1. the process is alive — `port_answers` alone cannot tell this daemon from a leaked one;
/// 2. `/metrics` answers 200, so the route table is up and the listener is ours;
/// 3. the daemon logged that it asserted the database version, which is the only evidence
///    available that it really reached the database this command prepared. **D-27** makes an
///    unreachable database a warning and a start anyway, so without this check the gate
///    would run just as happily against a daemon that never connected — and from M4-2 that
///    is the difference between "the contract is wrong" and "there was no data".
fn start_daemon(opts: &Options, config_db: &Path, procs: &mut Processes) -> Result<(), String> {
    say(&format!("Starting the daemon on 127.0.0.1:{REST_PORT}"));
    let mut cmd = Command::new(&opts.target_binary);
    cmd.args(["--config"])
        .arg(config_db)
        .args(["--log-format", "json"]);
    let daemon = procs.spawn(opts, "modernweather", cmd)?;
    let log = opts.log_dir.join("modernweather.log");

    await_for("the REST server", Duration::from_secs(60), || {
        // **Liveness first, readiness second.** A port answering is not evidence that THIS
        // daemon is answering it, and probing `http_ok` first meant the child's status was
        // never consulted on the success path -- so the liveness half the comment here used
        // to claim did not exist. `ports_free` runs in preflight, before a multi-minute
        // build, so the window is real: anything that takes 18080 during the build, a daemon
        // a previous `--keep-running` left included, would have been measured instead.
        //
        // Unreachable today by boot order in another file -- `boot.rs` binds before the
        // supervisor runs, so a child that lost the bind never logs SNAPSHOT_WARM and
        // `await_logged` below gives up -- but `boot.rs` records that M4-3 builds
        // `http:listener` as a component, after which the bind follows the OnStartup
        // components and that accident stops saving this.
        if procs.exited(daemon) {
            Wait::GiveUp("the daemon exited during startup".to_string())
        } else if http_ok(REST_PORT, "/metrics") {
            Wait::Ready
        } else {
            Wait::Pending
        }
    })
    .map_err(|e| format!("{e}. The daemon's log is {}", log.display()))?;

    // The message `crates/modernweather/src/boot.rs` logs on the success path of **D-62**.
    // Matched against the JSON rendering (`--log-format json` above) rather than the pretty
    // one, because that is the rendering with a stable field name — and if the message ever
    // changes, this fails loudly rather than quietly stopping to check.
    let text = std::fs::read_to_string(&log).map_err(|e| format!("{}: {e}", log.display()))?;
    if !text.contains(&format!("\"message\":\"{DB_ASSERTED}\"")) {
        return Err(format!(
            "the daemon came up but never logged that it asserted the database version, so it \
             did not reach {}.\n  \
             D-27 makes an unreachable database a warning and a start anyway, so a replay from \
             here would be measuring a daemon with no database — which is not what this gate \
             reports. Its log is {}",
            opts.db.database,
            log.display()
        ));
    }

    // And wait for the snapshot to be warm. `snapshot:warm` is `OnStartup` with a hard
    // `ReadingStore`, so it runs *after* `db:postgres` has connected -- which is after the
    // daemon is answering on `/metrics`. Without this the replay races it and reads a 404
    // from `/latest` for every station, which `judge_direct` would report as a frozen-class
    // contract break.
    await_for(
        "the snapshot table",
        Duration::from_secs(60),
        || match std::fs::read_to_string(&log) {
            Ok(text) if text.contains(SNAPSHOT_WARM) => Wait::Ready,
            Ok(_) if procs.exited(daemon) => {
                Wait::GiveUp("the daemon exited before the snapshot was warm".to_string())
            }
            Ok(_) => Wait::Pending,
            Err(e) => Wait::GiveUp(format!("{}: {e}", log.display())),
        },
    )
    .map_err(|e| {
        format!(
            "{e}.\n  `snapshot:warm` seeds the snapshot table from the weather database \
             (D-37), and `/latest` answers 404 for every station until it has. Its log is \
             {}; the component may be in `Waiting {{ on: ReadingStore }}` because \
             `db:postgres` could not connect.",
            log.display()
        )
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Step 4: the runner
// ---------------------------------------------------------------------------------------

/// Build `contract/conformance` with `--locked`.
///
/// A separate workspace on purpose (W1 Decision 7), so this is `--manifest-path` and its
/// own `target/`. `--locked` because the runner is the measuring instrument: a dependency
/// that resolved differently between two runs of a conformance suite is a changed
/// instrument, and `CONFORMANCE_BIN` is the supported way to supply a prebuilt one.
fn build_runner(opts: &Options) -> Result<PathBuf, String> {
    if let Some(prebuilt) = std::env::var_os("CONFORMANCE_BIN") {
        let path = PathBuf::from(prebuilt);
        if !path.is_file() {
            return Err(format!(
                "CONFORMANCE_BIN names {}, which is not a file",
                path.display()
            ));
        }
        return Ok(path);
    }
    say("Building the conformance runner");
    let mut cmd = cargo(&opts.root);
    cmd.args([
        "build",
        "--release",
        "--locked",
        "--manifest-path",
        "contract/conformance/Cargo.toml",
    ]);
    run(
        cmd,
        "cargo build --manifest-path contract/conformance/Cargo.toml",
    )?;
    let path = opts
        .root
        .join("contract/conformance/target/release/legacy-conformance");
    if !path.is_file() {
        return Err(format!("{} was not built", path.display()));
    }
    Ok(path)
}

/// One replay. The runner's own exit status is not consulted: it exits 1 for any failure
/// count, and the verdict this gate reports needs the counts rather than the bit.
///
/// Every replay is given [`DEVIATIONS`], the control included: the proxy forwards to the
/// same daemon, so the target is the same one and measuring it against a different
/// recording for the control would make the control's own floor meaningless.
fn replay(opts: &Options, runner: &Path, base_url: &str, tag: &str) -> Result<Summary, String> {
    let path = opts.workdir.join(format!("{tag}.txt"));
    let file = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    // One file, both streams: the runner prints its report on stdout and its progress on
    // stderr, and a failure is easier to read in one place. `try_clone` rather than two
    // `File::create` calls, so the two handles share one file offset instead of overwriting
    // each other.
    let err = file
        .try_clone()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut command = Command::new(runner);
    command.args(["--base-url", base_url]);
    // **Named, not inherited.** `count_fixtures` validated `<root>/contract/fixtures` and
    // `judge_direct` rests on `attempted == 70`; the runner defaults its directory from
    // `CONFORMANCE_FIXTURES`, so an exported variable -- the knob
    // `ci/conformance/README.md` documents -- had the gate validating one directory and
    // measuring another. Any 70 files satisfied the assertion. Passing the path and
    // clearing the variables makes the two the same directory by construction.
    command.args([
        "--fixtures",
        &opts.root.join("contract/fixtures").to_string_lossy(),
    ]);
    command.env_remove("CONFORMANCE_FIXTURES");
    command.env_remove("CONFORMANCE_BASE_URL");
    for (id, _) in DEVIATIONS {
        command.args(["--deviation", id]);
    }
    let status = command
        .current_dir(&opts.root)
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(err))
        .status()
        .map_err(|e| format!("running {}: {e}", runner.display()))?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Summary::parse(&text).map_err(|e| {
        format!(
            "{e}\n  It exited with {status}. Its output is {}",
            path.display()
        )
    })
}

fn start_proxy(opts: &Options, procs: &mut Processes, name: &str) -> Result<(), String> {
    let mut cmd = Command::new("python3");
    cmd.arg("contract/tools/rig/mangle_proxy.py");
    let proxy = procs.spawn(opts, name, cmd)?;
    await_for("the mangling proxy", Duration::from_secs(30), || {
        if port_answers(MANGLE_PORT) {
            Wait::Ready
        } else if procs.exited(proxy) {
            Wait::GiveUp("the mangling proxy exited without listening".to_string())
        } else {
            Wait::Pending
        }
    })
    .map_err(|e| {
        format!(
            "{e}. Its log is {}",
            opts.log_dir.join(format!("{name}.log")).display()
        )
    })
}

// ---------------------------------------------------------------------------------------
// Processes, ports and tools
// ---------------------------------------------------------------------------------------

/// Everything this command starts, killed on the way out — including on failure.
///
/// A leaked daemon owns 18080 and makes the next run fail for the wrong reason, which the
/// port preflight turns into a clear message rather than a silent measurement of the wrong
/// process. This is the `trap cleanup EXIT` of `go-target.sh`, as a `Drop`. It does not
/// cover a `SIGINT`, which Rust leaves to the default disposition: the preflight is the
/// safety net for that, same as it is there.
#[derive(Debug)]
struct Processes {
    started: Vec<Option<Child>>,
    keep_running: bool,
    verdict: bool,
}

/// An index into [`Processes`], so that a caller can ask about one process it started
/// without holding a borrow of the whole set.
#[derive(Copy, Clone, Debug)]
struct Handle(usize);

impl Processes {
    fn new(keep_running: bool) -> Processes {
        Processes {
            started: Vec::new(),
            keep_running,
            verdict: false,
        }
    }

    fn spawn(&mut self, opts: &Options, name: &str, mut cmd: Command) -> Result<Handle, String> {
        let path = opts.log_dir.join(format!("{name}.log"));
        let out = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let err = out
            .try_clone()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let child = cmd
            .current_dir(&opts.root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .map_err(|e| format!("starting {name}: {e}"))?;
        println!(
            "    started {name} (pid {}), log {}",
            child.id(),
            path.display()
        );
        self.started.push(Some(child));
        Ok(Handle(self.started.len() - 1))
    }

    /// Whether that process has already exited. Reaped here, so the status is remembered
    /// rather than asked for twice.
    fn exited(&mut self, h: Handle) -> bool {
        match &mut self.started[h.0] {
            None => true,
            Some(child) => match child.try_wait() {
                Ok(Some(_)) => {
                    self.started[h.0] = None;
                    true
                }
                Ok(None) => false,
                // An un-waitable child is not evidence that it is gone.
                Err(_) => false,
            },
        }
    }

    fn stop(&mut self, h: Handle) -> Result<(), String> {
        if let Some(mut child) = self.started[h.0].take() {
            let _ = child.kill();
            child
                .wait()
                .map_err(|e| format!("waiting for a child: {e}"))?;
        }
        Ok(())
    }

    /// `--keep-running` leaves everything up, and only once the replay reached a verdict:
    /// the processes of a run that measured nothing are a confusing inheritance for the next
    /// one, and the port preflight would reject them anyway.
    fn reached_a_verdict(&mut self) {
        self.verdict = true;
    }
}

impl Drop for Processes {
    fn drop(&mut self) {
        // Dropping a `Child` neither kills nor waits, so leaving them is simply not
        // touching them: they are reparented when this process exits and keep running.
        if self.keep_running && self.verdict {
            let live = self.started.iter().filter(|c| c.is_some()).count();
            println!("\n==> --keep-running: leaving {live} process(es) up");
            return;
        }
        for child in self.started.iter_mut().flatten() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn cargo(root: &Path) -> Command {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()));
    cmd.current_dir(root);
    cmd
}

/// Run a command to completion with its output on this process's own streams.
fn run(mut cmd: Command, what: &str) -> Result<(), String> {
    let status = cmd.status().map_err(|e| format!("running {what}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} exited with {status}"))
    }
}

/// Returns what the server said on stderr, which is where `RAISE NOTICE` and `RAISE
/// WARNING` land — the version assertion below says everything it has to say through those,
/// so a caller that swallowed them would run the check and discard its answer.
///
/// `-X` so that a developer's `~/.psqlrc` is not part of a measuring instrument; the
/// password through `PGPASSWORD` rather than the URL so it stays out of `argv`; and
/// `PGCONNECT_TIMEOUT` because without it a blackholed `PGHOST` blocks inside one call and
/// the caller's own deadline is never reached.
fn psql(opts: &Options, url: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("psql");
    cmd.args(["-X", "-v", "ON_ERROR_STOP=1", "-q", url])
        .args(args)
        .env("PGCONNECT_TIMEOUT", "5")
        .current_dir(&opts.root);
    if opts.db.password.is_empty() {
        cmd.env_remove("PGPASSWORD");
    } else {
        cmd.env("PGPASSWORD", &opts.db.password);
    }
    let out = cmd.output().map_err(|e| format!("running psql: {e}"))?;
    let stderr = String::from_utf8_lossy(&out.stderr).trim_end().to_string();
    if out.status.success() {
        return Ok(stderr);
    }
    Err(format!(
        "psql {} exited with {}:\n{stderr}",
        args.join(" "),
        out.status,
    ))
}

/// `psql` again, returning **stdout** rather than stderr.
///
/// Two functions rather than one returning both, because the two call sites want opposite
/// things and a tuple would make every one of them discard half of it: the version
/// assertion says everything it has to say through `RAISE NOTICE` on stderr, and a `SELECT`
/// says it on stdout.
fn psql_query(opts: &Options, url: &str, sql: &str) -> Result<String, String> {
    let mut cmd = Command::new("psql");
    cmd.args(["-X", "-v", "ON_ERROR_STOP=1", "-q", "-A", "-t", url])
        .args(["-c", sql])
        .env("PGCONNECT_TIMEOUT", "5")
        .current_dir(&opts.root);
    if opts.db.password.is_empty() {
        cmd.env_remove("PGPASSWORD");
    } else {
        cmd.env("PGPASSWORD", &opts.db.password);
    }
    let out = cmd.output().map_err(|e| format!("running psql: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    Err(format!(
        "psql -c {sql} exited with {}:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim_end()
    ))
}

/// `-bail` throughout: without it the `sqlite3` CLI prints `Parse error` and exits 0, so a
/// write that created nothing would look like one that worked. `.timeout` because the CLI's
/// busy timeout defaults to zero, which turns a lock still held into an immediate failure
/// rather than a short wait — both lessons are recorded in `go-target.sh`.
fn sqlite_write(db: &Path, sql: &str) -> Result<(), String> {
    sqlite3(db, sql).map(|_| ())
}

fn sqlite_query(db: &Path, sql: &str) -> Result<String, String> {
    sqlite3(db, sql)
}

fn sqlite3(db: &Path, sql: &str) -> Result<String, String> {
    // The SQL arrives on stdin rather than in `argv`, because one of these statements
    // carries the weather database's URL and a command line is world-readable in /proc.
    let mut child = Command::new("sqlite3")
        .args(["-bail", "-cmd", ".timeout 5000"])
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("running sqlite3: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "sqlite3's stdin was not piped".to_string())?
        .write_all(sql.as_bytes())
        .map_err(|e| format!("writing to sqlite3: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("waiting for sqlite3: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "sqlite3 exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim_end()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What one poll of [`await_for`] found.
#[derive(Debug)]
enum Wait {
    /// Stop waiting; the thing is there.
    Ready,
    /// Not yet. Try again until the deadline.
    Pending,
    /// Stop waiting; it is never going to be there, and here is why.
    GiveUp(String),
}

/// Poll until `poll` is `Ready`. Used instead of a fixed sleep so that a slow machine is
/// slow rather than broken, and a genuinely stuck step fails with the thing it was waiting
/// for named.
///
/// `GiveUp` is the half that is easy to leave out and expensive to leave out. Every caller
/// here is waiting on a child process, and a child that has exited will not become ready --
/// so without it the most likely failure (a daemon that died at startup) spends the whole
/// deadline and then reports "timed out", while the accurate message sits unreachable after
/// the call. Watched before it was fixed: a daemon that exited immediately took 72s to
/// produce `timed out after 60s waiting for the REST server`.
fn await_for(what: &str, deadline: Duration, mut poll: impl FnMut() -> Wait) -> Result<(), String> {
    let start = Instant::now();
    loop {
        match poll() {
            Wait::Ready => {
                println!(
                    "    {what} ready after {:.1}s",
                    start.elapsed().as_secs_f32()
                );
                return Ok(());
            }
            Wait::GiveUp(why) => return Err(why),
            Wait::Pending => {}
        }
        if start.elapsed() >= deadline {
            return Err(format!(
                "timed out after {}s waiting for {what}",
                deadline.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn port_answers(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// A 200 from `path`, over a hand-written request.
///
/// `xtask` has no HTTP client and is not getting one for a readiness probe: `DESIGN.md`
/// D-32's table is the list of crates this repository may reach for, and one request whose
/// only interesting property is its status line is eleven lines of `std`.
fn http_ok(port: u16, path: &str) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut sock) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
    let mut request = String::new();
    let _ = write!(
        request,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    if sock.write_all(request.as_bytes()).is_err() {
        return false;
    }
    // Twelve bytes, not fifteen with the trailing space: `HTTP/1.1 200\r\n` with no reason
    // phrase is a legal status line, and asserting the space would read it as not-ready.
    let mut head = [0u8; 12];
    if sock.read_exact(&mut head).is_err() {
        return false;
    }
    &head == b"HTTP/1.1 200"
}

fn which(tool: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(tool))
            .find(|p| p.is_file())
    })
}

fn say(what: &str) {
    println!("\n==> {what}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A summary with no per-fixture detail, for the tests about the preconditions --
    /// which are checked before the classification and therefore do not need one.
    fn summary(passed: usize, failed: usize, skipped: usize, transport: usize) -> Summary {
        Summary {
            passed,
            failed,
            skipped,
            transport,
            fixtures: Vec::new(),
        }
    }

    /// The deviation the shipped policy declares for `name`, if any. The helpers below
    /// apply it, because [`judge_direct`] now asserts the runner applied each declared
    /// deviation to exactly the fixtures that declare it -- so a synthetic report that
    /// omitted it would be rejected for the wrong reason.
    fn declared_deviation(name: &str) -> Option<String> {
        DEVIATIONS
            .iter()
            .find(|(_, fixtures)| fixtures.contains(&name))
            .map(|(id, _)| (*id).to_string())
    }

    fn verdict(name: &str, class: &str, passed: bool, skipped: bool) -> Fixture {
        Fixture {
            name: name.to_string(),
            class: class.to_string(),
            passed,
            skipped,
            deviation: declared_deviation(name),
        }
    }

    /// A summary whose per-fixture detail says what the classes did.
    ///
    /// The frozen and retired fixtures are drawn from [`FROZEN`] and [`RETIRED`] **in
    /// order**, rather than being named `frozen-N`: the gate now asserts every reported
    /// fixture's class against those rosters, so a synthetic name would be rejected as a
    /// misclassification. Coupled fixtures keep synthetic names, which is correct -- the
    /// roster's rule is that anything in neither list is coupled.
    fn classified(
        frozen_ok: usize,
        coupled_ok: usize,
        retired_failed: usize,
        failed: &[(&str, &str)],
    ) -> Summary {
        let mut fixtures = Vec::new();
        for name in FROZEN.iter().take(frozen_ok) {
            fixtures.push(verdict(name, "frozen", true, false));
        }
        for i in 0..coupled_ok {
            fixtures.push(verdict(&format!("coupled-{i}"), "coupled", true, false));
        }
        for name in RETIRED.iter().take(retired_failed) {
            fixtures.push(verdict(name, "retired", false, false));
        }
        for (name, class) in failed {
            fixtures.push(verdict(name, class, false, false));
        }
        let passed = fixtures.iter().filter(|f| f.passed).count();
        let failed_count = fixtures.iter().filter(|f| !f.passed && !f.skipped).count();
        Summary {
            passed,
            failed: failed_count,
            skipped: fixtures.len() - passed - failed_count,
            transport: 0,
            fixtures,
        }
    }

    /// Today's green shape: every frozen fixture passing, the thirteen pending website
    /// fixtures failing, the nine retired ones failing, and nothing else.
    /// [`green`], for a caller that mutates it. `Summary` is not `Clone` and does not need
    /// to be: rebuilding it is one call.
    fn green_clone() -> Summary {
        green()
    }

    fn green() -> Summary {
        let failed: Vec<(&str, &str)> = PENDING.iter().map(|(n, _)| (*n, "coupled")).collect();
        // 42 frozen, 19 coupled, 9 retired = 70. Since M4-3 emptied [`PENDING`] the second
        // term is all nineteen, which is Milestone 4's done-criterion; the expression is
        // left in terms of `PENDING` so that a future excuse needs no edit here.
        classified(42, 19 - PENDING.len(), 9, &failed)
    }

    /// The shape `contract/conformance` really prints, which is what the parser has to read
    /// back. Quoted from `run_all`: one line per fixture, a blank line, then the summary.
    const REAL_OUTPUT: &str = "\
replaying 70 fixtures from contract/fixtures against http://127.0.0.1:18080 with deviations DEV-10
FAIL latest-full [frozen]
     - status 404, expected 200
ok   stationinfo-full [frozen DEV-10]
SKIP alerts-empty [retired] (fixture stores no body to compare against)

class frozen   1 passed / 1 failed / 0 skipped
class retired  0 passed / 0 failed / 1 skipped
deviations applied: DEV-10

1 passed, 1 failed, 1 skipped
";

    #[test]
    fn the_summary_is_read_out_of_the_runners_own_output() {
        let s = Summary::parse(REAL_OUTPUT).expect("the summary line is there");
        assert_eq!((s.passed, s.failed, s.skipped, s.transport), (1, 1, 1, 0));
        assert_eq!(s.attempted(), 3);
        // The per-fixture verdicts, with the class tag work item N-18 added. The
        // `class frozen  …` lines are deliberately NOT read as verdicts, and the
        // `deviations applied:` line is not either.
        assert_eq!(
            s.fixtures,
            vec![
                Fixture {
                    name: "latest-full".into(),
                    class: "frozen".into(),
                    passed: false,
                    skipped: false,
                    deviation: None,
                },
                // The tag's second word, which the gate asserts the declared set against.
                Fixture {
                    name: "stationinfo-full".into(),
                    class: "frozen".into(),
                    passed: true,
                    skipped: false,
                    deviation: Some("DEV-10".into()),
                },
                // `SKIP`'s parenthesised reason is not a deviation id.
                Fixture {
                    name: "alerts-empty".into(),
                    class: "retired".into(),
                    passed: false,
                    skipped: true,
                    deviation: None,
                },
            ]
        );
        assert_eq!(s.failed_in("frozen"), vec!["latest-full"]);
        assert_eq!(
            s.failed_in("retired"),
            Vec::<&str>::new(),
            "a SKIP is neither a pass nor a failure"
        );
    }

    /// The per-fixture parser, on the lines it has to ignore as well as the ones it reads.
    #[test]
    fn only_the_three_verdict_lines_are_read_as_verdicts() {
        assert!(parse_fixture_line("     - status 404, expected 200").is_none());
        assert!(parse_fixture_line("class frozen   42 passed / 0 failed / 0 skipped").is_none());
        assert!(parse_fixture_line("deviations applied: DEV-10").is_none());
        assert!(parse_fixture_line("70 passed, 0 failed, 0 skipped").is_none());
        assert!(parse_fixture_line("").is_none());
        // A verdict with no class tag is NOT read, which is what makes an older runner an
        // honest `Inconclusive` rather than a gate that excuses everything.
        assert!(parse_fixture_line("ok   latest-full").is_none());
        assert_eq!(
            parse_fixture_line("ok   latest-full [coupled]"),
            Some(Fixture {
                name: "latest-full".into(),
                class: "coupled".into(),
                passed: true,
                skipped: false,
                deviation: None,
            })
        );
        assert_eq!(
            parse_fixture_line("FAIL stationinfo-full [frozen DEV-10]"),
            Some(Fixture {
                name: "stationinfo-full".into(),
                class: "frozen".into(),
                passed: false,
                skipped: false,
                deviation: Some("DEV-10".into()),
            })
        );
    }

    /// **The classification is pinned against the fixture files themselves**, in both
    /// directions. The counts this module's tests are built on -- 42 frozen, 19 coupled, 9
    /// retired -- are a property of `contract/fixtures/`, and the PENDING list is a claim
    /// about which of them no landed item implements.
    #[test]
    fn the_declared_classes_and_the_pending_list_match_the_fixtures() {
        let dir = crate::repo_root().join("contract/fixtures");
        let mut classes: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        let mut pending_in_files: Vec<String> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .expect("the fixtures are readable")
            .flatten()
        {
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("readable");
            // A four-line hand parse rather than a JSON dependency: `xtask` has three
            // dependencies and this is reading two string fields (**D-32**).
            let field = |key: &str| -> Option<String> {
                let needle = format!("\"{key}\": \"");
                let at = text.find(&needle)? + needle.len();
                let rest = &text[at..];
                Some(rest[..rest.find('"')?].to_string())
            };
            let name = field("name").unwrap_or_else(|| panic!("{}: no name", path.display()));
            let class = field("class")
                .unwrap_or_else(|| panic!("{}: no class (work item N-18)", path.display()));
            // **A name identifies a fixture, so it is pinned to the file that holds it.**
            // Every excuse this gate grants keys on the name: PENDING's lookup is a `find`,
            // and a fixture renamed onto a pending name would be excused by an entry that
            // never meant it -- with the class totals, the `pending` fields and the count of
            // 70 all still exact. Tying the name to the filename makes the collision
            // impossible rather than merely detected.
            let stem = path
                .file_stem()
                .expect("a fixture file has a stem")
                .to_string_lossy();
            assert_eq!(
                name,
                stem,
                "{}'s `name` is `{name}`. A fixture's name is what PENDING and DEVIATIONS \
                 key on, so it has to be the filename.",
                path.display()
            );
            assert!(
                !names.contains(&name),
                "two fixtures are named `{name}`, so a name does not identify one"
            );
            if field("pending").is_some() {
                pending_in_files.push(name.clone());
            }
            *classes.entry(class.clone()).or_default() += 1;
            // And the roster the gate carries, against the class the file declares -- the
            // other half of judge_direct's check, so a reclassification fails here too and
            // names the file.
            let declared = if FROZEN.contains(&name.as_str()) {
                "frozen"
            } else if RETIRED.contains(&name.as_str()) {
                "retired"
            } else {
                "coupled"
            };
            assert_eq!(
                class,
                declared,
                "{} is class `{class}` and FROZEN/RETIRED in this file say `{declared}`",
                path.display()
            );
            names.push(name);
        }
        assert_eq!(FROZEN.len(), 42);
        assert_eq!(RETIRED.len(), 9);
        assert_eq!(
            classes.get("frozen").copied().unwrap_or(0),
            42,
            "the five endpoints the iOS client calls, minus the three msgpack fixtures"
        );
        assert_eq!(
            classes.get("coupled").copied().unwrap_or(0),
            19,
            "/almanac, /api/stations, six HTML pages and seven assets"
        );
        assert_eq!(
            classes.get("retired").copied().unwrap_or(0),
            9,
            "/alerts, /api/remote-stations and the three msgpack fixtures (D-69)"
        );
        assert_eq!(classes.values().sum::<usize>(), EXPECTED_FIXTURES);

        pending_in_files.sort();
        let mut declared: Vec<String> = PENDING.iter().map(|(n, _)| (*n).to_string()).collect();
        declared.sort();
        assert_eq!(
            pending_in_files, declared,
            "the fixtures' own `pending` fields and this module's PENDING list have to be \
             the same set: the field documents it for a reader and the list is what \
             actually excuses a failure, and a disagreement means one of them is lying"
        );
        for (name, _) in PENDING {
            assert!(
                names.iter().any(|n| n == name),
                "PENDING names `{name}`, which is not a fixture"
            );
        }
    }

    /// Every deviation this gate enables has to be one a fixture declares, or the runner
    /// refuses the run -- so the constant and the fixtures are checked here rather than
    /// three minutes into a gate.
    ///
    /// **Both directions, by name.** The id alone was not enough: a deviation is a licence
    /// to compare against bytes the fixture's author wrote, so the question is not whether
    /// *something* declares it but whether exactly the fixtures this gate signed off on do.
    #[test]
    fn every_enabled_deviation_is_declared_by_exactly_the_fixtures_it_names() {
        let dir = crate::repo_root().join("contract/fixtures");
        for (id, named) in DEVIATIONS {
            let mut declaring: Vec<String> = Vec::new();
            for entry in std::fs::read_dir(&dir).expect("readable").flatten() {
                let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if text.contains(&format!("\"id\": \"{id}\"")) {
                    declaring.push(
                        entry
                            .path()
                            .file_stem()
                            .expect("a fixture file has a stem")
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
            declaring.sort();
            let mut want: Vec<String> = named.iter().map(|n| (*n).to_string()).collect();
            want.sort();
            assert_eq!(
                declaring, want,
                "`{id}` is declared by {declaring:?} and this gate names {want:?}. A \
                 deviation applied to a fixture nobody signed off on measures it against \
                 whatever its author typed; one named here and no longer declared is a \
                 stale record of what shipped."
            );
        }
    }

    #[test]
    fn a_connection_level_failure_is_counted_as_one() {
        // contract/conformance/src/lib.rs reports a transport error as an ordinary FAIL
        // line, so the only way to tell "the contract is wrong" from "nothing was
        // listening" is to read the reason.
        // Quoted from a real run: `legacy-conformance --base-url http://127.0.0.1:19999`
        // emits one of these per fixture, 70 FAIL lines all reading `request failed:`.
        let out = "FAIL alerts-active\n     - request failed: http://127.0.0.1:19999/alerts?\
                   station_id=1: Connection Failed: Connect error: Connection refused (os \
                   error 111)\n\n0 passed, 1 failed, 0 skipped\n";
        assert_eq!(Summary::parse(out).unwrap(), summary(0, 1, 0, 1));
    }

    #[test]
    fn a_run_that_printed_no_summary_is_an_error_rather_than_a_zero() {
        // A runner that exited 2 on a bad `--fixtures` prints only an error. Reading that
        // as "0 passed, 0 failed" would make an unrun suite indistinguishable from a
        // perfect one.
        let err = Summary::parse("error: cannot read contract/fixtures: No such file\n")
            .expect_err("no summary line");
        assert!(err.contains("did not finish a replay"), "{err}");
    }

    #[test]
    fn the_last_summary_line_wins_and_prose_is_not_one() {
        // Two streams land in one file, so the summary is found by shape rather than by
        // position -- and the shape has to be tight enough that a sentence mentioning the
        // words is not mistaken for it.
        let out = "70 passed, 0 failed, 0 skipped is what a green run prints\n\n\
                   4 passed, 66 failed, 0 skipped\n";
        assert_eq!(Summary::parse(out).unwrap(), summary(4, 66, 0, 0));
    }

    #[test]
    fn the_fixture_count_is_the_first_thing_asserted() {
        // M4-2 narrows the frozen surface to five endpoints, and the obvious way to
        // implement that is to replay fewer fixtures. This is the assertion that makes such
        // a change arrive as a red gate instead of a quieter green one.
        match judge_direct(&summary(0, 69, 0, 0), EXPECTED_FIXTURES, &PENDING) {
            Direct::Inconclusive(why) => {
                assert!(why.contains("attempted 69 fixtures, expected 70"), "{why}");
            }
            other => panic!("69 fixtures must not reach a verdict; got {other:?}"),
        }
    }

    /// **A frozen-class failure is a contract break and says so.** This is the verdict the
    /// gate gave on every fixture before M4-2 and the one it gives for a single regression
    /// on `/latest` now; the message has to name the fixtures, because "26 failed" does not
    /// tell a reader which of the three classes broke.
    #[test]
    fn a_frozen_class_failure_is_a_contract_break_and_exits_1() {
        let mut s = green();
        // One frozen fixture regresses, which is the whole of a contract break. Found by
        // name rather than renamed into place: green() draws its frozen fixtures from the
        // FROZEN roster now, so a rename would collide with the real entry.
        let i = s
            .fixtures
            .iter()
            .position(|f| f.name == "latest-full-complement")
            .expect("it is a frozen fixture");
        s.fixtures[i].passed = false;
        s.passed -= 1;
        s.failed += 1;
        match judge_direct(&s, EXPECTED_FIXTURES, &PENDING) {
            Direct::NotConforming(why) => {
                assert!(why.contains("FROZEN-class"), "{why}");
                assert!(why.contains("latest-full-complement"), "{why}");
                assert!(why.contains("contract break"), "{why}");
            }
            other => panic!("a frozen failure is a conformance failure; got {other:?}"),
        }
    }

    /// A coupled-class failure nothing excuses is `NotConforming` too, and the message says
    /// what coupled means: a lockstep change, not an unconstrained one.
    #[test]
    fn an_unexcused_coupled_failure_is_also_a_conformance_failure() {
        let mut s = green();
        // One of the six coupled fixtures this item DOES implement regresses.
        let coupled = s
            .fixtures
            .iter_mut()
            .find(|f| f.class == "coupled" && f.passed)
            .expect("the six implemented coupled fixtures");
        coupled.name = "almanac-populated".into();
        coupled.passed = false;
        s.passed -= 1;
        s.failed += 1;
        match judge_direct(&s, EXPECTED_FIXTURES, &PENDING) {
            Direct::NotConforming(why) => {
                assert!(why.contains("COUPLED-class"), "{why}");
                assert!(why.contains("almanac-populated"), "{why}");
                assert!(
                    !why.contains("html-privacy"),
                    "the excused ones are not named: {why}"
                );
            }
            other => panic!("got {other:?}"),
        }
    }

    /// **The retired nine never gate.** `/alerts`, `/api/remote-stations` and the three
    /// msgpack fixtures are not carried forward (**D-69**) and their fixtures are kept only
    /// so that the replay still attempts all 70.
    #[test]
    fn the_retired_class_fails_and_the_gate_is_still_green() {
        assert_eq!(
            judge_direct(&green(), EXPECTED_FIXTURES, &PENDING),
            Direct::Conforms
        );
    }

    /// A fixture excused without a declaration would make this gate narrower than it says,
    /// and a declaration whose fixture has started passing would make it excuse a failure
    /// that cannot happen. Both are exit 2.
    ///
    /// **Over a synthetic list, because the real one is empty.** M4-3 emptied [`PENDING`],
    /// and a test that read it would now assert nothing -- so [`judge_direct`] takes the
    /// list as a parameter and the production call site passes `&PENDING`. That is the only
    /// way this property stays checked for whatever is excused next; a both-directions check
    /// that cannot fail is worth exactly as much as a gate that cannot.
    #[test]
    fn the_pending_list_is_checked_in_both_directions() {
        // `classified` names its coupled fixtures `coupled-<n>`, so the synthetic entry
        // names one of those rather than a real fixture: what is under test is the
        // bookkeeping, not which page is excused.
        const SYNTHETIC: [(&str, &str); 1] = [("coupled-0", "M9-9")];

        // Stale: a declared fixture passed, which is what M4-3 landing looked like.
        let green = green();
        assert!(
            green
                .fixtures
                .iter()
                .any(|f| f.name == SYNTHETIC[0].0 && f.passed),
            "the synthetic entry has to name a fixture that passes, or `stale` is vacuous"
        );
        match judge_direct(&green, EXPECTED_FIXTURES, &SYNTHETIC) {
            Direct::Inconclusive(why) => {
                assert!(why.contains("declared pending and PASSED"), "{why}");
                assert!(why.contains(SYNTHETIC[0].0), "{why}");
            }
            other => panic!("a stale excuse proves less than it claims; got {other:?}"),
        }

        // Missing: a declared fixture the runner did not report, which is a rename. The
        // replacement **passes**, so that the unexcused-coupled check does not fire first
        // and the message under test is the one about the declaration.
        let mut s = green_clone();
        s.fixtures.retain(|f| f.name != SYNTHETIC[0].0);
        s.fixtures
            .push(verdict("coupled-renamed", "coupled", true, false));
        match judge_direct(&s, EXPECTED_FIXTURES, &SYNTHETIC) {
            Direct::Inconclusive(why) => {
                assert!(why.contains("did not report it at all"), "{why}");
                assert!(why.contains(SYNTHETIC[0].0), "{why}");
            }
            other => panic!("got {other:?}"),
        }

        // And the excuse working: the declared fixture fails and the gate still passes,
        // which is the state M4-3 inherited and removed.
        let mut s = green_clone();
        for f in &mut s.fixtures {
            if f.name == SYNTHETIC[0].0 {
                f.passed = false;
            }
        }
        s.passed -= 1;
        s.failed += 1;
        assert_eq!(
            judge_direct(&s, EXPECTED_FIXTURES, &SYNTHETIC),
            Direct::Conforms,
            "a declared failure is excused, which is what the list is for"
        );
        // The same failure with the real, empty list is a contract break.
        match judge_direct(&s, EXPECTED_FIXTURES, &PENDING) {
            Direct::NotConforming(why) => assert!(why.contains("COUPLED-class"), "{why}"),
            other => panic!("nothing excuses it any more; got {other:?}"),
        }
    }

    /// A report with no class tags is an older runner against a newer gate, and reading it
    /// as unclassified would excuse every coupled-class failure.
    #[test]
    fn a_report_without_class_tags_reaches_no_verdict() {
        match judge_direct(&summary(70, 0, 0, 0), EXPECTED_FIXTURES, &PENDING) {
            Direct::Inconclusive(why) => assert!(why.contains("class tag"), "{why}"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_daemon_that_was_not_answering_reaches_no_verdict() {
        // The same 70 failures as the case above, for a completely different reason. If
        // these two were one verdict, "the daemon did not start" would read as "the
        // rewrite does not match the contract" for the whole of M3.
        match judge_direct(&summary(0, 70, 0, 70), EXPECTED_FIXTURES, &PENDING) {
            Direct::Inconclusive(why) => assert!(why.contains("transport failures"), "{why}"),
            other => panic!("transport failures prove nothing; got {other:?}"),
        }
    }

    #[test]
    fn a_clean_replay_does_not_pass_on_its_own() {
        // W1 Decision 10: a green conformance run is only accepted alongside a passing
        // negative control, so the clean case is a question rather than an answer.
        assert_eq!(
            judge_direct(&green(), EXPECTED_FIXTURES, &PENDING),
            Direct::Conforms
        );
    }

    /// A SKIP is "the fixture stores no body", not "the fixture did not run", so it is part
    /// of the 70 and it is not a failure -- **and it is not a pass either**.
    ///
    /// This test asserted `Conforms` until the body-comparison hole was found. It was the
    /// hole: the counts put a skip in neither bucket and `failed_in` filters it out, so one
    /// `frozen` fixture losing its recorded body took the 133-key DTO, both duplicate key
    /// pairs and every `omitempty` decision out of the comparison and the gate still printed
    /// green. The all-skipped check only catches losing every body at once.
    #[test]
    fn a_skipped_frozen_fixture_is_not_a_pass() {
        let mut s = green();
        for i in 0..2 {
            s.fixtures[i].passed = false;
            s.fixtures[i].skipped = true;
        }
        s.passed -= 2;
        s.skipped += 2;
        match judge_direct(&s, EXPECTED_FIXTURES, &PENDING) {
            Direct::Inconclusive(why) => {
                assert!(
                    why.contains("frozen-class fixture(s) were SKIPPED"),
                    "{why}"
                );
                assert!(why.contains(FROZEN[0]), "it names which: {why}");
            }
            other => panic!("a frozen body nobody compared is not a verdict; got {other:?}"),
        }
        // Still counted in the 70, which is the half of the original claim that was right.
        assert_eq!(s.attempted(), EXPECTED_FIXTURES);
    }

    /// The same skip in `retired`, which is allowed: nothing gates on those either way, and
    /// three of them are `format=msgpack` responses D-69 does not carry forward at all.
    #[test]
    fn a_skipped_retired_fixture_is_fine() {
        let mut s = green();
        let i = s
            .fixtures
            .iter()
            .position(|f| f.class == "retired")
            .expect("green() has retired fixtures");
        s.fixtures[i].passed = false;
        s.fixtures[i].skipped = true;
        s.failed -= 1;
        s.skipped += 1;
        assert_eq!(
            judge_direct(&s, EXPECTED_FIXTURES, &PENDING),
            Direct::Conforms
        );
    }

    /// The roster, in the direction that matters: a frozen fixture moved to `retired` stops
    /// gating, and the class totals can be kept exact by moving something back the other
    /// way. Without the roster check the gate reads one word in one JSON file.
    #[test]
    fn a_frozen_fixture_relabelled_retired_is_refused() {
        let mut s = green();
        s.fixtures[0].class = "retired".into();
        match judge_direct(&s, EXPECTED_FIXTURES, &PENDING) {
            Direct::Inconclusive(why) => {
                assert!(why.contains("class this gate does not declare"), "{why}");
                assert!(why.contains(FROZEN[0]), "it names which: {why}");
            }
            other => panic!("a reclassification must not be silent; got {other:?}"),
        }
    }

    /// Two fixtures reporting one name makes [`PENDING`]'s `find` ambiguous, so an
    /// unrelated failure can be excused by an entry that never meant it.
    #[test]
    fn a_duplicate_fixture_name_is_refused() {
        let mut s = green();
        s.fixtures[1].name = s.fixtures[0].name.clone();
        match judge_direct(&s, EXPECTED_FIXTURES, &PENDING) {
            Direct::Inconclusive(why) => assert!(why.contains("was reported twice"), "{why}"),
            other => panic!("got {other:?}"),
        }
    }

    /// A deviation applied where this gate does not declare it means the fixture was
    /// measured against bytes its author wrote. `main.rs` catches the typo direction; this
    /// is the one that would ship.
    #[test]
    fn a_deviation_applied_to_an_undeclared_fixture_is_refused() {
        let mut s = green();
        let i = s
            .fixtures
            .iter()
            .position(|f| f.name == "latest-full-complement")
            .expect("green() has it");
        s.fixtures[i].deviation = Some("DEV-10".into());
        match judge_direct(&s, EXPECTED_FIXTURES, &PENDING) {
            Direct::Inconclusive(why) => {
                assert!(why.contains("this gate does not declare it for"), "{why}");
                assert!(why.contains("latest-full-complement"), "{why}");
            }
            other => panic!("got {other:?}"),
        }
    }

    /// And the other way: a declared deviation the runner did not apply means the fixture
    /// lost its deviation block, so the entry is now stale and the record of what shipped is
    /// wrong.
    #[test]
    fn a_declared_deviation_that_was_not_applied_is_refused() {
        let mut s = green();
        let i = s
            .fixtures
            .iter()
            .position(|f| f.name == "stationinfo-about-text-omitted")
            .expect("green() has it");
        s.fixtures[i].deviation = None;
        match judge_direct(&s, EXPECTED_FIXTURES, &PENDING) {
            Direct::Inconclusive(why) => {
                assert!(why.contains("did not apply it to"), "{why}");
                assert!(why.contains("stationinfo-about-text-omitted"), "{why}");
            }
            other => panic!("got {other:?}"),
        }
    }

    /// A replay with per-class detail: the first `frozen_ok` of [`FROZEN`] and `coupled_ok`
    /// coupled fixtures passing, everything else in the 70 failing. The control's floor is
    /// now per class, so a summary carrying only counts cannot be judged.
    fn control_shape(frozen_ok: usize, coupled_ok: usize) -> Summary {
        let mut fixtures = Vec::new();
        for (i, name) in FROZEN.iter().enumerate() {
            fixtures.push(verdict(name, "frozen", i < frozen_ok, false));
        }
        for i in 0..19 {
            fixtures.push(verdict(
                &format!("coupled-{i}"),
                "coupled",
                i < coupled_ok,
                false,
            ));
        }
        for name in RETIRED {
            fixtures.push(verdict(name, "retired", false, false));
        }
        let passed = fixtures.iter().filter(|f| f.passed).count();
        Summary {
            passed,
            failed: fixtures.len() - passed,
            skipped: 0,
            transport: 0,
            fixtures,
        }
    }

    /// The measured shape: 48 of the 70 served directly, 25 still passing through the
    /// proxy, so 23 broken -- 20 of them frozen.
    #[test]
    fn a_negative_control_that_holds_is_what_makes_a_green_run_green() {
        let direct = control_shape(42, 6);
        assert_eq!((direct.passed, direct.failed), (48, 22));
        match judge_control(
            &direct,
            &control_shape(22, 3),
            EXPECTED_FIXTURES,
            MIN_MANGLED_BROKEN,
            MIN_FROZEN_MANGLED_BROKEN,
        ) {
            Control::Held(note) => {
                assert!(note.contains("23 of the 48"), "{note}");
                assert!(note.contains("20 of 42 frozen"), "{note}");
                assert!(note.contains("25 fixtures still passed"), "{note}");
            }
            other => panic!("got {other:?}"),
        }
    }

    /// **The floor that will matter next.** `implementation-plan.md` M4-3 makes all 19
    /// coupled fixtures pass, after which a proxy that breaks the website's responses and
    /// leaves the five frozen endpoints alone clears a total floor of 20 while asserting
    /// nothing about the contract deployed clients depend on. Stated now rather than
    /// discovered when the total stops binding.
    #[test]
    fn a_control_that_only_broke_the_websites_responses_is_rejected() {
        // The post-M4-3 shape: 42 frozen and all 19 coupled served.
        let direct = control_shape(42, 19);
        // The proxy broke all 19 coupled and exactly one frozen: 20 broken, which clears
        // the overall floor on its own.
        match judge_control(
            &direct,
            &control_shape(41, 0),
            EXPECTED_FIXTURES,
            MIN_MANGLED_BROKEN,
            MIN_FROZEN_MANGLED_BROKEN,
        ) {
            Control::Broken(why) => {
                assert!(why.contains("FROZEN-class"), "{why}");
                assert!(
                    why.contains("clears the overall floor"),
                    "the message has to say the total was not the problem: {why}"
                );
            }
            other => panic!("a control blind to the frozen half must not hold; got {other:?}"),
        }
    }

    /// **The case the re-derivation exists for.** A proxy that mangles nothing leaves the
    /// 22 always-failing fixtures failing, which clears `contract/README.md`'s floor of 26
    /// against the Go target only because that target passes all 70 -- here it would clear
    /// a failure floor and prove nothing. Measuring what the proxy BROKE is what rejects it.
    #[test]
    fn a_proxy_that_mangled_nothing_is_rejected_even_though_twenty_two_fixtures_failed() {
        let direct = control_shape(42, 6);
        // The proxy forwarded faithfully: exactly the same verdicts as the direct replay.
        match judge_control(
            &direct,
            &control_shape(42, 6),
            EXPECTED_FIXTURES,
            MIN_MANGLED_BROKEN,
            MIN_FROZEN_MANGLED_BROKEN,
        ) {
            Control::Broken(why) => {
                assert!(why.contains("broke only 0"), "{why}");
                assert!(
                    why.contains("not of fixtures that failed"),
                    "the message has to say why a failure count would not do: {why}"
                );
            }
            other => panic!("a proxy that changed nothing must not hold; got {other:?}"),
        }
    }

    /// A proxy that somehow made MORE fixtures pass is not a control either: something
    /// other than the proxy changed between the two replays.
    #[test]
    fn a_control_that_fixed_responses_is_rejected() {
        match judge_control(
            &control_shape(42, 6),
            &control_shape(42, 8),
            EXPECTED_FIXTURES,
            MIN_MANGLED_BROKEN,
            MIN_FROZEN_MANGLED_BROKEN,
        ) {
            Control::Broken(why) => assert!(why.contains("FIXED responses"), "{why}"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn every_way_the_negative_control_can_fail_to_bite_is_rejected() {
        // Every one of these is a run that would print "the control held" under a bare
        // "at least 26 failed" floor, which is the defect the three-part assertion exists
        // to close. None of them may be reachable from a green gate.
        let direct = control_shape(42, 6);
        let cases = [
            // Everything passed through the proxy, which fires before either floor.
            (summary(70, 0, 0, 0), "PASSED"),
            (summary(0, 70, 0, 70), "transport failures"),
            (control_shape(0, 0), "including the ones it does not mangle"),
            (control_shape(40, 6), "broke only 2"),
            (summary(10, 26, 0, 0), "attempted 36 fixtures"),
        ];
        for (s, expected) in cases {
            match judge_control(
                &direct,
                &s,
                EXPECTED_FIXTURES,
                MIN_MANGLED_BROKEN,
                MIN_FROZEN_MANGLED_BROKEN,
            ) {
                Control::Broken(why) => assert!(why.contains(expected), "{s:?}: {why}"),
                Control::Held(note) => panic!("{s:?} must not hold: {note}"),
            }
        }
    }

    /// The counts in this file are the contract, so they are checked against the tree
    /// rather than against memory. W1 froze 70 fixtures; a set that changed size is a
    /// contract that changed, and this is where that gets noticed.
    #[test]
    fn the_tree_really_holds_seventy_fixtures() {
        let root = crate::repo_root();
        assert_eq!(count_fixtures(&root).unwrap(), EXPECTED_FIXTURES);
    }

    /// `ci/gates.toml` names this command and the path it replays against. A gate whose
    /// `cmd` drifted from the runner would report the owner's name instead of a verdict
    /// (`xtask::unknown_subcommand`), which is a confusing way to find out.
    #[test]
    fn the_gate_declares_the_command_this_module_implements() {
        let gates = crate::gates::load().expect("ci/gates.toml parses");
        let gate = gates
            .gates
            .iter()
            .find(|g| g.id == "conformance")
            .expect("ci/gates.toml declares the conformance gate");
        assert_eq!(
            gate.cmd.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "cargo",
                "xtask",
                "conformance",
                "--target-binary",
                "target/debug/modernweather"
            ],
            "the gate's command is the one xtask implements"
        );
        // The default has to agree with the gate, or `cargo xtask conformance` with no
        // arguments would measure a different binary than CI does.
        let opts = Options::parse(&crate::repo_root(), &[])
            .expect("no arguments is valid")
            .expect("no arguments is a run, not a help request");
        assert_eq!(
            opts.target_binary,
            crate::repo_root().join("target/debug/modernweather")
        );
    }

    #[test]
    fn the_options_are_parsed_and_a_typo_is_refused() {
        let root = crate::repo_root();
        let opts = Options::parse(
            &root,
            &[
                "--release".into(),
                "--no-build".into(),
                "--workdir".into(),
                "scratch/c".into(),
            ],
        )
        .expect("a valid command line")
        .expect("a command line that runs");
        assert!(opts.release);
        assert!(!opts.build);
        assert_eq!(opts.workdir, root.join("scratch/c"));
        // --release moves the default, so the pair cannot disagree by accident.
        assert_eq!(
            opts.target_binary,
            root.join("target/release/modernweather")
        );
        assert_eq!(opts.log_dir, root.join("scratch/c/logs"));

        let err = Options::parse(&root, &["--target-bin".into(), "x".into()]).unwrap_err();
        assert!(err.contains("unknown option `--target-bin`"), "{err}");
    }

    /// Every way a command line can be wrong that would otherwise measure something other
    /// than what the caller asked for, or delete something they did not ask about.
    #[test]
    fn a_command_line_that_would_measure_the_wrong_thing_is_refused() {
        let root = crate::repo_root();
        let cases: [(&[&str], &str); 6] = [
            // `flag_value` returns None for both "absent" and "present with nothing after
            // it", so without this the typo silently measured target/debug/modernweather.
            (&["--target-binary"], "`--target-binary` needs a value"),
            (&["--workdir"], "`--workdir` needs a value"),
            (&["--workdir="], "empty value"),
            // Passed the unknown-option check and then matched nothing, so it was ignored.
            (&["--no-build=yes"], "takes no value"),
            // `Path::join` replaces the base when the argument is absolute, which pointed
            // clear_workdir's remove_dir_all at /var/logs.
            (&["--workdir", "/var"], "is absolute"),
            (&["--workdir", "../../etc"], "escapes it"),
        ];
        for (args, expected) in cases {
            let owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
            let err = Options::parse(&root, &owned)
                .err()
                .unwrap_or_else(|| panic!("{args:?} should be refused"));
            assert!(err.contains(expected), "{args:?}: {err}");
        }
    }

    #[test]
    fn help_prints_and_does_not_run() {
        for flag in ["-h", "--help"] {
            let parsed = Options::parse(&crate::repo_root(), &[flag.to_string()])
                .expect("help is not an error");
            assert!(parsed.is_none(), "{flag} should print and stop");
        }
    }

    #[test]
    fn the_database_url_carries_the_environment_and_satisfies_the_schemas_check() {
        let db = Libpq {
            host: "db.internal".into(),
            port: "5433".into(),
            user: "mw".into(),
            password: "hunter2".into(),
            database: "mw_conformance".into(),
            sslmode: "require".into(),
        };
        assert_eq!(
            db.target_url(),
            "postgresql://mw:hunter2@db.internal:5433/mw_conformance?sslmode=require"
        );
        // The CHECK this has to satisfy is read out of the schema rather than restated
        // here, the same way the rig test reads mangle_proxy.py: a constraint quoted into a
        // comment stops being an assertion the moment the schema changes.
        let schema = std::fs::read_to_string(
            crate::repo_root().join("migrations/config/20261102230000_config_schema_initial.sql"),
        )
        .expect("the configuration schema is in the tree");
        let check = schema
            .lines()
            .find(|l| l.contains("url") && l.contains("GLOB"))
            .expect("weather_database.url still has a GLOB check on its scheme");
        let scheme = db
            .target_url()
            .split("://")
            .next()
            .expect("a URL has a scheme")
            .to_string();
        assert!(
            check.contains(&format!("'{scheme}://?*'")),
            "the seeded URL's scheme `{scheme}` is not one the schema accepts: {check}"
        );

        let anonymous = Libpq {
            password: String::new(),
            ..db
        };
        // No empty `:` before the `@`: libpq accepts it, and a URL with a password-shaped
        // hole in it is harder to read in a log.
        assert_eq!(
            anonymous.target_url(),
            "postgresql://mw@db.internal:5433/mw_conformance?sslmode=require"
        );
    }

    /// The daemon goes where `mangle_proxy.py` already looks, and that file is W1's. If the
    /// proxy is ever moved, this says so rather than the negative control quietly
    /// forwarding to nothing.
    #[test]
    fn the_ports_are_the_ones_the_rig_hard_codes() {
        let proxy =
            std::fs::read_to_string(crate::repo_root().join("contract/tools/rig/mangle_proxy.py"))
                .expect("the rig is in the tree");
        assert!(
            proxy.contains(&format!("(\"127.0.0.1\", {REST_PORT})")),
            "mangle_proxy.py no longer forwards to {REST_PORT}"
        );
        assert!(
            proxy.contains(&format!("(\"127.0.0.1\", {MANGLE_PORT})")),
            "mangle_proxy.py no longer listens on {MANGLE_PORT}"
        );
    }

    /// The literal `start_daemon` greps the daemon's log for. A reworded `info!` would
    /// otherwise turn this gate's one piece of evidence that the daemon reached the database
    /// into a 60-second wait and a confusing exit 2 — the same class of coupling as the rig
    /// ports, and the only one that had no guard.
    #[test]
    fn the_daemon_still_logs_the_message_this_gate_waits_for() {
        let boot =
            std::fs::read_to_string(crate::repo_root().join("crates/modernweather/src/boot.rs"))
                .expect("the boot sequence is in the tree");
        assert!(
            boot.contains(&format!("\"{DB_ASSERTED}\"")),
            "crates/modernweather/src/boot.rs no longer logs `{DB_ASSERTED}`, so \
             start_daemon's check can never pass"
        );
    }

    /// A canned server, so that the hand-written request and the status-line comparison are
    /// pinned rather than reasoned about. The 404 case is the one that matters: the daemon
    /// answers 404 on every route this gate replays, and a probe that accepted one would
    /// report a daemon with no route table as ready.
    #[test]
    fn the_readiness_probe_accepts_only_a_200() {
        use std::io::{BufRead as _, BufReader};
        use std::net::TcpListener;

        for (status, expected) in [
            ("HTTP/1.1 200 OK", true),
            // A legal status line with no reason phrase. The probe used to require the
            // trailing space after the code and would have read this as not-ready.
            ("HTTP/1.1 200", true),
            ("HTTP/1.1 404 Not Found", false),
            ("HTTP/1.1 503 Service Unavailable", false),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port");
            let port = listener.local_addr().expect("a bound address").port();
            // `clippy.toml` forbids `std::thread::spawn` so that §3.2's task table owns
            // every task in the DAEMON; an allow with a comment at the call site is the
            // mechanism that file prescribes for a permitted one. This is an xtask test
            // serving one canned response to the probe under test — there is no task table
            // here, and `accept()` has to block somewhere other than where `http_ok` calls
            // `connect()`.
            #[allow(clippy::disallowed_methods)]
            let server = std::thread::spawn(move || {
                let (sock, _) = listener.accept().expect("the probe connects");
                // The request has to be read, or the probe's write can fail with EPIPE
                // before it ever looks at the response.
                let mut reader = BufReader::new(sock);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let body = format!("{status}\r\nContent-Length: 0\r\n\r\n");
                let _ = reader.into_inner().write_all(body.as_bytes());
            });
            assert_eq!(
                http_ok(port, "/metrics"),
                expected,
                "`{status}` should read as ready={expected}"
            );
            server.join().expect("the canned server finishes");
        }
    }

    /// A port already answering is the one thing that makes this gate measure the wrong
    /// process, which is why the preflight exists and why `go-target.sh` records the same
    /// lesson. Checked against a port the test itself owns rather than 18080, so a
    /// conformance run in another terminal cannot fail this.
    #[test]
    fn a_port_already_answering_stops_the_run() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port");
        let port = listener.local_addr().expect("a bound address").port();
        let err = ports_free(&[port]).expect_err("a bound port must be refused");
        assert!(
            err.contains(&format!("port {port} is already in use")),
            "{err}"
        );
        drop(listener);
        ports_free(&[port]).expect("a released port is free again");
    }

    /// The restored-cache defence. `Swatinem/rust-cache` restores `target/` wholesale, so
    /// the default work directory arrives holding the previous run's replay output — and an
    /// artefact from a run that did not happen is exactly the kind of small lie this gate
    /// exists to prevent.
    #[test]
    fn a_previous_runs_output_does_not_survive_into_this_one() {
        let workdir = check_tmp_workdir();
        let logs = workdir.join("logs");
        std::fs::create_dir_all(&logs).expect("a scratch work directory");
        for stale in ["config.db", "config.db-wal", "direct.txt", "mangled.txt"] {
            std::fs::write(workdir.join(stale), "from an earlier run").expect("a stale file");
        }
        std::fs::write(logs.join("modernweather.log"), "an earlier run's log").expect("a log");

        let opts = Options {
            root: crate::repo_root(),
            target_binary: PathBuf::new(),
            build: false,
            release: false,
            log_dir: logs.clone(),
            workdir: workdir.clone(),
            keep_running: false,
            db: Libpq::from_env(),
        };
        clear_workdir(&opts).expect("the work directory is cleared");

        for gone in ["config.db", "config.db-wal", "direct.txt", "mangled.txt"] {
            assert!(
                !workdir.join(gone).exists(),
                "{gone} survived into this run"
            );
        }
        assert!(
            !logs.join("modernweather.log").exists(),
            "a previous run's log survived into this one"
        );
        assert!(
            logs.is_dir(),
            "the log directory is left ready to write into"
        );
        let _ = std::fs::remove_dir_all(&workdir);
    }

    /// Unique per call: `cargo test` runs these concurrently in one process.
    fn check_tmp_workdir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "mw-conformance-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn credentials_are_percent_encoded_into_the_url() {
        // Unencoded, `postgresql://postgres:p@ss/word@host:5432/db` has a host of `ss`:
        // sqlx would connect somewhere else entirely and D-27 would turn that into a
        // warning rather than a failure.
        let db = Libpq {
            host: "db.internal".into(),
            port: "5432".into(),
            user: "mw admin".into(),
            password: "p@ss/word".into(),
            database: "mw_conformance".into(),
            sslmode: "disable".into(),
        };
        assert_eq!(
            db.target_url(),
            "postgresql://mw%20admin:p%40ss%2Fword@db.internal:5432/mw_conformance?sslmode=disable"
        );
        // The password never goes on a command line; `psql` is handed it in its environment.
        assert!(
            !db.url("postgres").contains("p%40ss"),
            "{}",
            db.url("postgres")
        );
    }

    #[test]
    fn a_database_that_is_not_named_for_this_run_is_not_dropped() {
        // The guard is on the name because the statement is DROP DATABASE ... WITH (FORCE)
        // and the name comes from the environment.
        for refused in ["weather", "postgres", "remoteweather", ""] {
            let err = droppable(refused).expect_err("this name must be refused");
            assert!(
                err.contains(&format!("refusing to drop `{refused}`")),
                "{err}"
            );
            assert!(
                err.contains("mw_conformance"),
                "the default is named: {err}"
            );
        }
        // Both halves of the suite already name their database this way.
        for accepted in ["mw_conformance", "conformance", "chris_conformance_scratch"] {
            droppable(accepted).unwrap_or_else(|e| panic!("`{accepted}`: {e}"));
        }
        // The name this command uses when PGDATABASE is unset has to be one of them, or the
        // guard would refuse every ordinary run.
        droppable(&Libpq::from_env().database).expect("the configured database is acceptable");
    }

    #[test]
    fn a_replay_where_everything_skipped_is_not_a_pass() {
        // A SKIP means "the fixture stores no body". All 70 skipping means the fixtures
        // lost their recorded responses, which is not the binary conforming.
        match judge_direct(&summary(0, 0, 70, 0), EXPECTED_FIXTURES, &PENDING) {
            Direct::Inconclusive(why) => assert!(why.contains("were SKIPPED"), "{why}"),
            other => panic!("70 skips is not a verdict; got {other:?}"),
        }
    }

    /// The jobs the single required `gate` check depends on, read out of the workflow.
    ///
    /// Hand-parsed because `xtask` has three dependencies and none of them is a YAML
    /// reader (**D-32**). Narrow on purpose: the `needs:` list of one named job, and
    /// nothing else in the file.
    fn gate_job_needs() -> Vec<String> {
        let workflow = std::fs::read_to_string(crate::repo_root().join(".github/workflows/pr.yml"))
            .expect("the PR workflow is in the tree");
        let job = workflow
            .split_once("\n  gate:\n")
            .expect("pr.yml declares a `gate` job")
            .1;
        let list = job
            .split_once("\n    needs:\n")
            .expect("the gate job declares `needs`")
            .1;
        list.lines()
            .take_while(|l| l.starts_with("      - "))
            .map(|l| l.trim_start_matches("      - ").to_string())
            .collect()
    }

    /// The `conformance` gate is red by design until M4-2, so M2-5 took it out of the one
    /// required check — a check nobody may merge past for two milestones is one everybody
    /// learns to override, which is `docs/reconciliation.md` G-1 by a different route.
    ///
    /// The risk with that is the obvious one: an exception nobody removes. This is the
    /// mechanism rather than a third comment about it (G-5 again) — the exception and the
    /// plan's instruction to undo it are asserted to be *the same decision*, so the two
    /// halves cannot drift apart. Re-adding the job without striking the plan's sentence
    /// fails here, and so does striking the sentence while the job is still out.
    #[test]
    fn the_required_check_excludes_this_gate_exactly_while_the_plan_says_to() {
        let needs = gate_job_needs();
        assert!(
            needs.contains(&"conformance-fixtures".to_string()),
            "the Go-target half is not red by design and must stay required: {needs:?}"
        );
        let excluded = !needs.contains(&"conformance".to_string());

        let plan = std::fs::read_to_string(crate::repo_root().join("docs/implementation-plan.md"))
            .expect("the implementation plan is in the tree");
        // The sentence M4-2's done-criteria carry, which is what puts the line back.
        let plan_says_restore = plan.contains("goes back into the `gate` job's `needs`");

        assert_eq!(
            excluded, plan_says_restore,
            "the workflow and the plan disagree about the one required-check exception.\n  \
             `conformance` excluded from the gate job's needs: {excluded}\n  \
             M4-2's done-criteria require it to be restored: {plan_says_restore}\n  \
             Both or neither: if the gate is green now, re-add the job AND strike that \
             sentence from the plan in the same change."
        );
    }

    /// `migrations/` and `migrations/config/` are two databases on two engines (**D-8** as
    /// amended by A-W6-2), so the weather half must not pick up the SQLite half's DDL.
    #[test]
    fn the_migration_listing_does_not_descend_into_the_second_database() {
        let root = crate::repo_root();
        let dir = root.join("migrations");
        let weather = sql_files(&dir).unwrap();
        assert!(!weather.is_empty());
        assert!(
            weather.iter().all(|p| p.parent() == Some(dir.as_path())),
            "the weather list reached into a subdirectory: {weather:?}"
        );
        assert!(
            !sql_files(&root.join("migrations/config"))
                .unwrap()
                .is_empty(),
            "the configuration migrations are found by the same helper"
        );
        // Filename order is the applied order, and it is the only thing that makes a
        // migration sequence a sequence.
        let mut sorted = weather.clone();
        sorted.sort();
        assert_eq!(weather, sorted);
    }
}
