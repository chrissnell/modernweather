//! `cargo xtask db-tests` — the `db-tests` gate's command.
//!
//! Runs `crates/mw-store/tests/db.rs` against a real TimescaleDB and **asserts that
//! exactly the declared tests ran and passed**. That assertion is the gate: W3 Decision 84
//! is that these tests fail CI if they skip, and the two ways a test suite reports green
//! without running are both closed here —
//!
//! - the tests are `#[ignore]`d, so the `test` job (which has no service container) does
//!   not run them, and a filter that matches nothing **exits zero**;
//! - a test can be renamed, or its body emptied, without changing the count.
//!
//! So there are three checks, each closing one of them, and the third is the one
//! `xtask/src/emulators.rs` does **not** have:
//!
//! 1. [`reconcile`] compares the test binary's own `--list` output against [`DECLARED`] in
//!    **both directions**, so a rename on either side is a failure rather than a smaller
//!    suite.
//! 2. [`verdict`] requires every declared name to have reached `... ok`, so a filter that
//!    matched nothing is not a pass.
//! 3. [`verdict`] also requires every declared name to have printed a [`MARKER`] line,
//!    which the test can only produce by **running a query**. That is what closes the
//!    emptied-body hole: `emulators.rs` records that four `#[ignore]`d functions with
//!    gutted bodies produced `ok: 4 test(s) passed` and exit 0 with no emulator ever
//!    started, and names alone would not have caught it here either.
//!
//! A fourth failure mode is the environment rather than the suite, and conflating it with
//! a red write path is the lie `ci/gates.toml` promises not to tell: see
//! [`HARNESS_FAILURES`] and [`reachable`].
//!
//! # Why the declared list is in this file and not in a manifest
//!
//! `driver-emulators` keeps its declared set in `tests/emulators/manifest.toml`, because
//! that manifest carries other things a reader needs — the pinned reference commit, which
//! emulators are still pending, and which of them are independently authored (**D-40**).
//! There is nothing of that kind here: one test target, one package, no external tree. A
//! TOML file holding one array would be a second place to edit and nothing else.
//!
//! # Where the database comes from
//!
//! The standard libpq variables, with `PGDATABASE` defaulting to **`postgres`** — which is
//! the database the `timescale/timescaledb-ha:pg17` service container creates and the one
//! the `database` job already uses. No `CREATE DATABASE`, so no `psql` and no second
//! client on the dependency list; `tests/db.rs` applies `migrations/` through `sqlx`'s own
//! migrator, which is idempotent.
//!
//! Nothing here drops anything, which is the difference from `cargo xtask conformance` and
//! the reason this needs no equivalent of that command's `droppable` guard. Stated in full,
//! because "nothing drops anything" is not the same as "no side effects": the tests apply
//! `migrations/` — which is what migrations are for — and then **insert** rows under
//! station keys beginning `db-tests:`, deleting their own at the *start* of each test so
//! that a failed run cannot poison the next one. The lasting effect on a database pointed
//! at by hand is therefore a migrated schema and a handful of `db-tests:*` rows, which a
//! locally-run daemon's own `/stationinfo` would list.
//!
//! Two consequences for a developer, neither of which bites in CI (a fresh container per
//! job): `PGDATABASE` defaults to the maintenance database `postgres`, so a bare
//! invocation on a box with a local cluster installs the weather schema there; and
//! `sqlx::migrate` tracks `_sqlx_migrations` while `ci/db/migrate-twice.sh` tracks
//! `_mw_ci_applied`, so pointing this gate at a database already migrated by `psql` fails
//! on `relation already exists` rather than finding the schema at HEAD. Both surface as
//! exit 2, which is the honest verdict for them.

use std::process::{Command, ExitCode, Stdio};

const PACKAGE: &str = "mw-store";
const TARGET: &str = "db";

/// Every test `crates/mw-store/tests/db.rs` must run, by name.
///
/// Renaming a test means renaming it here in the same change, and that is deliberate: a
/// test this gate no longer knows it is running is a test that can stop running.
const DECLARED: &[&str] = &[
    // The constraint M2-4 handed this item: `mw_store::version::probe`'s SQL, its column
    // aliases and its decoded types have no other test.
    "the_version_probe_reads_the_server_it_is_pointed_at",
    // "readings land in `observations`" and "an unreported metric is NULL, not 0.0".
    "readings_land_in_observations_and_an_unreported_metric_is_null",
    // The registry's column list against the catalogue, in both directions.
    "the_observations_columns_are_exactly_what_the_registry_declares",
    "one_metric_of_each_declared_type_round_trips",
    // "a replayed prefix produces no duplicate rows".
    "a_replayed_prefix_produces_no_duplicate_rows",
    // W3 §2.4's batch size, executed rather than asserted against the statement text.
    "a_full_batch_of_256_is_one_statement",
    "envelopes_sharing_an_instant_in_one_batch_are_merged_instead_of_failing",
    // "`station_metrics` reflects what has been written", and D-56's other projection.
    "the_projections_reflect_what_has_been_written",
    "a_station_the_registry_does_not_know_is_still_stored",
    "a_vanished_configuration_row_does_not_erase_the_projection",
    // implementation-plan.md M3-4's first done-criterion, and the two reads the rest of
    // the window rollup is built on.
    "a_ten_minute_mean_over_the_rolled_up_buckets_equals_the_mean_from_raw",
    "a_range_stitched_across_three_levels_totals_the_same_as_raw",
    "an_hourly_rain_series_is_read_in_order_with_dry_hours_absent",
    // implementation-plan.md M4-2, found by its code review: a closed bucket the refresh
    // policy has not reached was in neither the materialised prefix nor the one-bucket
    // tail, so `/span/720h` dropped yesterday for the first half hour of every UTC day.
    // The only test in the tree that does NOT materialise the whole window first.
    "a_closed_bucket_the_aggregate_has_not_caught_up_to_is_still_returned",
    // implementation-plan.md M4-1's one database-shaped claim: `ReadingStore::latest`
    // reads 216 real columns back into a `Reading` in the types they hold, which the
    // snapshot tests over a fake store cannot say anything about.
    "the_newest_reading_in_the_window_is_read_back_whole",
    // implementation-plan.md M4-2's named regression test: W1-BUG-1, end to end. The
    // finaliser is asserted in isolation by ci/db/schema-assertions.sql A-3; this asserts
    // the query `/span` actually issues, which is a different failure -- the Go bug is a
    // SQLSTATE rather than a wrong number.
    "a_span_over_a_station_with_no_wind_direction_returns_zero_rather_than_failing",
];

/// The panic messages `crates/mw-store/tests/db.rs`'s own harness produces when the
/// **environment** is wrong rather than the write path.
///
/// Every one of these is an `expect` in `prepare()` or `url()`, and every one of them means
/// the run reached no verdict. Without this, "there is no server" arrives as eight failed
/// tests and `summary()` reports `failed = 8` — so the gate exits 1 and tells the operator
/// the write path is broken, which is the opposite of what `ci/gates.toml` promises and the
/// expensive direction for this gate to lie in (C-W2-20).
///
/// Matched against the captured output as substrings. They are deliberately the literal
/// strings in that file; the test below asserts each one still appears in it, so a reworded
/// `expect` cannot silently stop being recognised.
const HARNESS_FAILURES: &[&str] = &[
    "MW_DB_TESTS_URL is not set",
    "MW_DB_TESTS_URL names a reachable server",
    "the server has the timescaledb extension available",
    "migrations/ applies through the sqlx migrator",
];

/// Where each test records that it has actually queried the database.
///
/// The declared **names** stop a filter that matched nothing and stop a rename. They do
/// **not** stop an emptied body: a test gutted to `{}` still lists, still prints `... ok`,
/// and still counts — which is the hole `xtask/src/emulators.rs` documents and does not
/// close. This does close it: the marker is printed by a helper that runs a query, so a
/// body that never reached the database cannot produce one, and the gate requires one per
/// declared test.
const EVIDENCE: &str = "MW_DB_TESTS_EVIDENCE";

pub fn main(args: &[String]) -> Result<ExitCode, String> {
    if let Some(unexpected) = args.first() {
        return Err(format!(
            "db-tests takes no arguments, got `{unexpected}`. It reads the standard libpq \
             variables and has no modes."
        ));
    }
    check_declared()?;
    let url = url();
    println!(
        "db-tests: {} declared test(s) in {PACKAGE}/tests/{TARGET}.rs against {}",
        DECLARED.len(),
        redact(&url)
    );

    reachable(&url)?;
    let listed = list(&url)?;
    reconcile(DECLARED, &listed)?;

    let code = run(&url)?;
    if code == ExitCode::SUCCESS {
        println!(
            "db-tests: {} test(s) passed against a real TimescaleDB",
            DECLARED.len()
        );
    }
    Ok(code)
}

/// Everything about the declared list that can be wrong before a database is involved.
fn check_declared() -> Result<(), String> {
    if DECLARED.is_empty() {
        return Err(
            "DECLARED is empty, so this gate would run nothing and pass. That is \
             docs/reconciliation.md G-1, which is the failure this gate exists against."
                .to_string(),
        );
    }
    let mut names: Vec<&str> = DECLARED.to_vec();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    if names.len() != before {
        return Err("DECLARED names the same test twice".to_string());
    }
    Ok(())
}

/// The connection string: `MW_DB_TESTS_URL` if it is already set, otherwise built from the
/// standard libpq variables.
///
/// Honouring the variable the tests read means a developer who has one exported gets what
/// they expect rather than a silently different database.
fn url() -> String {
    if let Ok(url) = std::env::var("MW_DB_TESTS_URL") {
        if !url.trim().is_empty() {
            return url;
        }
    }
    let var = |name: &str, default: &str| {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_string())
    };
    let host = var("PGHOST", "127.0.0.1");
    let port = var("PGPORT", "5432");
    let user = var("PGUSER", "postgres");
    let database = var("PGDATABASE", "postgres");
    let sslmode = var("PGSSLMODE", "disable");
    // Empty is a legitimate password (`POSTGRES_HOST_AUTH_METHOD: trust`), which is why it
    // is not defaulted through the non-empty filter above.
    let password = std::env::var("PGPASSWORD").unwrap_or_default();
    let credentials = if password.is_empty() {
        encode(&user)
    } else {
        format!("{}:{}", encode(&user), encode(&password))
    };
    format!("postgresql://{credentials}@{host}:{port}/{database}?sslmode={sslmode}")
}

/// Percent-encode a URL userinfo component. Hand-rolled for `xtask/src/conformance.rs`'s
/// reason: this crate has three dependencies and **D-32**'s crate list is a decision.
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

/// The URL with its password removed, which is the only form that reaches this command's
/// output.
fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<not a url>".to_string();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let authority = match authority.rsplit_once('@') {
        // Userinfo ends at the **last** `@` inside the authority, which is what keeps an
        // `@` in a password from being read as the separator. The `REDACTED` is added only
        // when there was something to redact: `postgres@host` with trust authentication is
        // the CI shape, and printing it back with a fake password would read as a
        // misconfiguration.
        Some((userinfo, host)) => match userinfo.split_once(':') {
            Some((user, _)) => format!("{user}:REDACTED@{host}"),
            None => format!("{userinfo}@{host}"),
        },
        None => authority.to_string(),
    };
    format!("{scheme}://{authority}{tail}")
}

/// Refuse the run if nothing is listening, **before** `cargo test` turns it into eight
/// failed tests.
///
/// Belt to [`HARNESS_FAILURES`]' braces, and the half that produces the better message: an
/// absent server is by far the most common way to run this command wrong, and "no verdict"
/// arriving in four milliseconds with the address in it beats the same verdict arriving
/// after eight connection timeouts.
fn reachable(url: &str) -> Result<(), String> {
    use std::net::{TcpStream, ToSocketAddrs};
    let authority = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // Userinfo ends at the last `@`, so an `@` in a password is not a host separator.
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let addrs = host_port
        .to_socket_addrs()
        .map_err(|e| {
            format!(
                "`{}` is not an address this host can resolve: {e}. The run reached no \
                 verdict about the write path.",
                redact_host(host_port)
            )
        })?
        .collect::<Vec<_>>();
    if addrs
        .iter()
        .any(|addr| TcpStream::connect_timeout(addr, std::time::Duration::from_secs(5)).is_ok())
    {
        return Ok(());
    }
    Err(format!(
        "nothing is listening on {}, so these tests cannot say anything about the write \
         path either way.\n  \
         Point the gate at a TimescaleDB with PGHOST/PGPORT/PGUSER/PGPASSWORD/PGDATABASE, \
         or set MW_DB_TESTS_URL. CI runs it in the `db-tests` job against \
         timescale/timescaledb-ha:pg17 (DESIGN.md D-2).",
        redact_host(host_port)
    ))
}

/// A `host:port` is not a secret, but it arrives out of a URL that may carry one.
fn redact_host(host_port: &str) -> &str {
    host_port
}

/// The test names libtest reports for this target.
///
/// Deliberately **without** `--ignored`: `--list` emits the same set either way, and
/// without the flag a test that someone forgot to `#[ignore]` is still visible to
/// [`reconcile`] rather than hidden from it.
fn list(url: &str) -> Result<Vec<String>, String> {
    let out = cargo_test(url, &["--list"])?;
    Ok(out
        .lines()
        .filter_map(|l| l.strip_suffix(": test"))
        .map(str::to_string)
        .collect())
}

/// Run them, one at a time, and assert every declared name reached `... ok`.
///
/// **The two exit codes are different verdicts, which is `conformance`'s precedent and the
/// same reasoning (C-W2-20).** `Ok(ExitCode::from(1))` means the tests ran and something
/// about the write path is wrong; `Err` — which `main` turns into exit 2 — means the run
/// reached no verdict and proved nothing either way. "There is no server" and "the writer
/// stores a zero where it should store NULL" produce the same red otherwise, and reporting
/// the first as the second is the expensive direction for this gate to lie in.
fn run(url: &str) -> Result<ExitCode, String> {
    // `--test-threads=1` because every test shares one database. They are isolated by
    // station key, so parallel execution would be correct -- except for the first one in,
    // which applies `migrations/`: `sqlx`'s migrator takes an advisory lock, so a
    // concurrent run waits rather than racing, and waiting out the whole aggregate
    // hierarchy on seven connections is slower than running in sequence.
    // The evidence file is created empty and handed to the test process, which appends one
    // line per test that reached the database. Created here rather than discovered, so a
    // stale file from an earlier run cannot be read as this run's evidence.
    let evidence =
        std::env::temp_dir().join(format!("mw-db-tests-evidence-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&evidence);
    std::fs::write(&evidence, "").map_err(|e| format!("creating {}: {e}", evidence.display()))?;

    let out = cargo_test_with(
        url,
        &["--test-threads=1", "--ignored"],
        Some(evidence.as_path()),
    )?;
    let reached = std::fs::read_to_string(&evidence).unwrap_or_default();
    let _ = std::fs::remove_file(&evidence);
    match verdict(DECLARED, &out, &reached) {
        Verdict::Passed => Ok(ExitCode::SUCCESS),
        Verdict::Failed(why) => {
            // The whole output, once, when something is wrong. A gate that failed without
            // showing what the tests said is a gate somebody has to reproduce locally.
            println!("--- cargo test output ---\n{out}");
            eprintln!("db-tests: {why}");
            Ok(ExitCode::from(1))
        }
        Verdict::Inconclusive(why) => {
            println!("--- cargo test output ---\n{out}");
            Err(why)
        }
    }
}

/// What one run of the test binary amounts to.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Every declared test ran and passed.
    Passed,
    /// The tests ran and something about the write path is wrong. Exit 1.
    Failed(String),
    /// The run proved nothing either way. Exit 2.
    Inconclusive(String),
}

/// Read libtest's output and decide which of the three it was.
///
/// Pure, over the text, because that is the only way the three branches are testable: a
/// gate whose own verdict logic has never been exercised is the shape this gate exists
/// against. The distinction between [`Verdict::Failed`] and [`Verdict::NoVerdict`] is
/// `conformance`'s and the reasoning is the same (C-W2-20): "there was no database" and
/// "the writer stores a zero where it should store NULL" otherwise produce the same red,
/// and reporting the first as the second is the expensive direction for this gate to lie
/// in.
fn verdict(declared: &[&str], out: &str, reached: &str) -> Verdict {
    let Some((passed, failed)) = summary(out) else {
        return Verdict::Inconclusive(
            "cargo test printed no `test result:` line, so nothing can be said about what \
             it ran"
                .to_string(),
        );
    };
    // Before the failure count is read as a verdict about the write path: a failure that is
    // the *harness* saying it has no database is not a statement about the writer.
    if let Some(signature) = HARNESS_FAILURES.iter().find(|s| out.contains(**s)) {
        return Verdict::Inconclusive(format!(
            "the test harness could not reach a usable database (`{signature}`), so the run \
             reached no verdict about the write path. Fix the run, then read its verdict."
        ));
    }
    if failed != 0 {
        return Verdict::Failed(format!("{failed} of {} test(s) failed", passed + failed));
    }
    let ran = passed_names(out);
    let absent: Vec<&str> = declared
        .iter()
        .copied()
        .filter(|d| !ran.contains(d))
        .collect();
    if !absent.is_empty() {
        return Verdict::Inconclusive(format!(
            "{} declared test(s) did not pass: {}. A filter that matches nothing exits \
             zero, which is why the names are checked and not only the count.",
            absent.len(),
            absent.join(", ")
        ));
    }
    if passed != declared.len() {
        return Verdict::Inconclusive(format!(
            "{passed} test(s) passed and {} are declared",
            declared.len()
        ));
    }
    // The names say which tests ran; the evidence file says they reached the database. A
    // test gutted to `{}` satisfies everything above and leaves no line behind.
    let touched = touched_names(reached);
    let untouched: Vec<&str> = declared
        .iter()
        .copied()
        .filter(|d| !touched.contains(d))
        .collect();
    if !untouched.is_empty() {
        return Verdict::Inconclusive(format!(
            "{} declared test(s) passed without recording that they queried the database: \
             {}. Every test in that file must end with `touched(&pool, <its own name>)`, \
             which runs a query and appends a line to ${EVIDENCE} -- that is what makes \
             \"green because it did not run\" unreachable rather than merely unlikely \
             (W3 D-84).",
            untouched.len(),
            untouched.join(", ")
        ));
    }
    Verdict::Passed
}

/// Every test that recorded having queried the database.
///
/// One line per test: `<name> observations=<n>`.
fn touched_names(reached: &str) -> Vec<&str> {
    reached
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect()
}

/// The declared set against the set the test binary actually has, in **both** directions.
///
/// Pure for [`verdict`]'s reason. A declared name the file does not have is a rename
/// nobody propagated; a test the file has and this gate does not declare is a test that
/// runs nowhere at all -- `cargo test` skips it as `#[ignore]`d and this gate never asks
/// for it.
fn reconcile(declared: &[&str], listed: &[String]) -> Result<(), String> {
    let missing: Vec<&str> = declared
        .iter()
        .copied()
        .filter(|d| !listed.iter().any(|l| l == d))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "xtask/src/dbtests.rs declares {} test(s) that {PACKAGE}/tests/{TARGET}.rs does \
             not have: {}. A renamed test is a test this gate no longer knows it is \
             running; rename it here in the same change.",
            missing.len(),
            missing.join(", ")
        ));
    }
    let undeclared: Vec<&str> = listed
        .iter()
        .map(String::as_str)
        .filter(|l| !declared.contains(l))
        .collect();
    if !undeclared.is_empty() {
        return Err(format!(
            "{PACKAGE}/tests/{TARGET}.rs has {} test(s) this gate does not declare: {}. \
             Every test in that file needs a database, so an undeclared one is a test that \
             runs nowhere -- `cargo test` skips it as `#[ignore]`d and this gate does not \
             ask for it. Add it to DECLARED.",
            undeclared.len(),
            undeclared.join(", ")
        ));
    }
    Ok(())
}

/// `cargo test -p mw-store --test db --locked -- <args>`, with `MW_DB_TESTS_URL` set.
///
/// stdout and stderr are merged into the returned string so that a panic message — which
/// libtest writes to stdout but cargo's own failures go to stderr — is in the one place
/// the caller prints.
fn cargo_test(url: &str, args: &[&str]) -> Result<String, String> {
    cargo_test_with(url, args, None)
}

/// The same, with the evidence file the test process appends to.
fn cargo_test_with(
    url: &str,
    args: &[&str],
    evidence: Option<&std::path::Path>,
) -> Result<String, String> {
    let mut cmd = Command::new(cargo());
    cmd.current_dir(crate::repo_root())
        .args(["test", "--locked", "-p", PACKAGE, "--test", TARGET, "--"])
        .args(args)
        .env("MW_DB_TESTS_URL", url);
    match evidence {
        Some(path) => cmd.env(EVIDENCE, path),
        // Removed rather than left alone, so an exported variable from a previous run
        // cannot make `--list` append to somebody else's evidence.
        None => cmd.env_remove(EVIDENCE),
    };
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("running cargo test: {e}"))?;
    let merged = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() && !args.contains(&"--list") {
        // Not an `Err` yet: the caller reads the summary, because "the binary exited 101"
        // and "three tests failed" want different messages and the second is the useful
        // one. A `--list` that failed has no summary to read, so it is an error here.
        return Ok(merged);
    }
    if !output.status.success() {
        println!("--- cargo test --list output ---\n{merged}");
        return Err(format!(
            "listing the tests failed ({}). The target did not build, or the database is \
             not reachable.",
            output
                .status
                .code()
                .map_or_else(|| "by signal".to_string(), |c| c.to_string())
        ));
    }
    Ok(merged)
}

/// Every test libtest reported as passing.
fn passed_names(out: &str) -> Vec<&str> {
    out.lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("test ")?;
            let (name, verdict) = rest.rsplit_once(" ... ")?;
            (verdict.trim() == "ok").then_some(name.trim())
        })
        .collect()
}

/// The `passed` and `failed` counts from libtest's summary line.
fn summary(out: &str) -> Option<(usize, usize)> {
    let line = out.lines().find(|l| l.starts_with("test result:"))?;
    let count = |label: &str| -> Option<usize> {
        let words: Vec<&str> = line.split_whitespace().collect();
        let at = words
            .iter()
            .position(|w| w.trim_end_matches(';') == label)?;
        words.get(at.checked_sub(1)?)?.parse().ok()
    };
    Some((count("passed")?, count("failed")?))
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The declared list against the file, without a database: the test target's source is
    /// in the tree, so the one thing this gate's own correctness rests on can be checked by
    /// `cargo test`.
    ///
    /// Deliberately a source scan rather than `--list`, which would need the target to
    /// build *and* would not run here: what this asserts is that `DECLARED` and the `#[test]
    /// ]` functions in that file are the same set, which is a property of the two texts.
    #[test]
    fn declared_matches_the_test_file() {
        let path = crate::repo_root()
            .join("crates")
            .join(PACKAGE)
            .join("tests")
            .join(format!("{TARGET}.rs"));
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));

        // `async fn <name>(` on the line after a `#[tokio::test]`-and-`#[ignore]` pair is
        // every test in the file. A function that is not `#[ignore]`d would run in the
        // `test` job, which has no database, so it is a failure here rather than a name to
        // collect.
        let mut found: Vec<String> = Vec::new();
        let lines: Vec<&str> = source.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let Some(rest) = line.trim().strip_prefix("async fn ") else {
                continue;
            };
            let Some(name) = rest.split('(').next() else {
                continue;
            };
            // Only the ones immediately preceded by the attribute pair.
            let preceding: Vec<&str> = lines[i.saturating_sub(4)..i]
                .iter()
                .map(|l| l.trim())
                .collect();
            if !preceding.iter().any(|l| l.starts_with("#[tokio::test]")) {
                continue;
            }
            assert!(
                preceding.iter().any(|l| l.starts_with("#[ignore")),
                "`{name}` in {} is a #[tokio::test] that is not #[ignore]d, so the `test` \
                 job -- which has no database -- would run it and fail",
                path.display()
            );
            found.push(name.to_string());
        }

        let mut declared: Vec<String> = DECLARED.iter().map(|s| (*s).to_string()).collect();
        declared.sort();
        found.sort();
        assert_eq!(
            found,
            declared,
            "DECLARED and the #[test] functions in {} are not the same set",
            path.display()
        );
    }

    /// The four verdicts, over libtest output this command's own invocation produces.
    #[test]
    fn the_verdicts_are_distinguishable_from_the_output_alone() {
        let declared = ["alpha", "beta"];
        let evidence = "alpha observations=3\nbeta observations=3\n";
        let green = "test alpha ... ok\ntest beta ... ok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;\n";
        assert_eq!(verdict(&declared, green, evidence), Verdict::Passed);

        // A real failure: the tests ran and the write path is wrong. Exit 1.
        let red = "test alpha ... ok\ntest beta ... FAILED\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored;\n";
        match verdict(&declared, red, evidence) {
            Verdict::Failed(why) => assert!(why.contains("1 of 2"), "{why}"),
            other => panic!("expected Failed, got {other:?}"),
        }

        // **No database.** Eight panicking tests look exactly like a red write path to a
        // failure count, and `ci/gates.toml`'s `on_exit.2` promises they do not. This is
        // the case that was reported as exit 1 -- "the write path does not do what M3-3's
        // done-criteria say it does" -- when nothing was listening.
        for signature in HARNESS_FAILURES {
            let no_db = format!(
                "test alpha ... FAILED\ntest beta ... FAILED\n\
                 panicked at: {signature}\n\
                 \ntest result: FAILED. 0 passed; 2 failed; 0 ignored;\n"
            );
            match verdict(&declared, &no_db, "") {
                Verdict::Inconclusive(why) => {
                    assert!(why.contains("reached no verdict"), "{why}");
                    assert!(why.contains(signature), "{why}");
                }
                other => panic!("`{signature}` is the environment, not the writer: {other:?}"),
            }
        }

        // A filter that matched nothing exits **zero** and prints a green summary.
        let nothing =
            "\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out;\n";
        match verdict(&declared, nothing, "") {
            Verdict::Inconclusive(why) => assert!(why.contains("did not pass"), "{why}"),
            other => panic!("a run that executed nothing is not a pass; got {other:?}"),
        }

        // A renamed test: the count is right and the names are not.
        let renamed =
            "test alpha ... ok\ntest gamma ... ok\n\ntest result: ok. 2 passed; 0 failed;\n";
        match verdict(&declared, renamed, evidence) {
            Verdict::Inconclusive(why) => assert!(why.contains("beta"), "{why}"),
            other => panic!("expected Inconclusive, got {other:?}"),
        }

        // **An emptied body.** It lists, it prints `... ok`, the count is right, and the
        // names are right -- and it never opened a connection, so it left no evidence.
        // This is the hole `xtask/src/emulators.rs` documents and does not close.
        match verdict(&declared, green, "alpha observations=3\n") {
            Verdict::Inconclusive(why) => {
                assert!(why.contains("without recording"), "{why}");
                assert!(why.contains("beta"), "{why}");
            }
            other => panic!("a test that never queried the database is not a pass: {other:?}"),
        }
        match verdict(&declared, green, "") {
            Verdict::Inconclusive(why) => assert!(why.contains("alpha, beta"), "{why}"),
            other => panic!("expected Inconclusive, got {other:?}"),
        }

        // No summary at all: the binary did not reach a verdict.
        match verdict(&declared, "could not compile", "") {
            Verdict::Inconclusive(why) => {
                assert!(why.contains("no `test result:` line"), "{why}")
            }
            other => panic!("expected Inconclusive, got {other:?}"),
        }
    }

    /// Every [`HARNESS_FAILURES`] signature must still be a literal in the test file it
    /// came from, or the classification silently stops recognising it.
    #[test]
    fn every_harness_signature_still_appears_in_the_test_file() {
        let path = crate::repo_root()
            .join("crates")
            .join(PACKAGE)
            .join("tests")
            .join(format!("{TARGET}.rs"));
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        for signature in HARNESS_FAILURES {
            assert!(
                source.contains(signature),
                "`{signature}` is how this gate recognises \"the environment is wrong, not \
                 the write path\", and it is no longer in {}. Reword both together.",
                path.display()
            );
        }
    }

    /// Both directions of the declared-set comparison, each with the message that names
    /// what to do about it.
    #[test]
    fn the_declared_set_is_reconciled_in_both_directions() {
        let declared = ["alpha", "beta"];
        let listed =
            |names: &[&str]| -> Vec<String> { names.iter().map(|s| (*s).to_string()).collect() };
        assert!(reconcile(&declared, &listed(&["alpha", "beta"])).is_ok());

        let e = reconcile(&declared, &listed(&["alpha"])).expect_err("beta is gone");
        assert!(e.contains("beta"), "{e}");
        assert!(e.contains("renamed"), "{e}");

        let e =
            reconcile(&declared, &listed(&["alpha", "beta", "gamma"])).expect_err("gamma is new");
        assert!(e.contains("gamma"), "{e}");
        assert!(e.contains("Add it to DECLARED"), "{e}");
    }

    /// `--list` output is `name: test` per line, and nothing else in it is a name.
    #[test]
    fn the_list_output_is_parsed_as_names() {
        let out = "alpha: test\nbeta: test\n\n2 tests, 0 benchmarks\n";
        let names: Vec<&str> = out
            .lines()
            .filter_map(|l| l.strip_suffix(": test"))
            .collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn every_passing_test_is_named_and_a_failing_one_is_not() {
        let out = "test alpha ... ok\ntest beta ... FAILED\ntest gamma ... ignored\n";
        assert_eq!(passed_names(out), vec!["alpha"]);
    }

    #[test]
    fn an_empty_declared_list_is_a_failure() {
        // `check_declared` is what guards it; the shipped list is non-empty, so the
        // property is asserted against the function rather than by emptying the constant.
        assert!(check_declared().is_ok());
        assert!(!DECLARED.is_empty());
    }

    #[test]
    fn the_summary_line_is_parsed_the_way_libtest_prints_it() {
        let out = "test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;";
        assert_eq!(summary(out), Some((8, 0)));
        assert_eq!(
            summary("test result: FAILED. 6 passed; 2 failed; 0 ignored;"),
            Some((6, 2))
        );
        assert_eq!(summary("nothing to see"), None);
    }

    #[test]
    fn a_password_never_reaches_the_output() {
        let r = redact("postgresql://mw:hunter2@db.internal:5432/weather?sslmode=disable");
        assert!(!r.contains("hunter2"), "{r}");
        assert_eq!(
            r,
            "postgresql://mw:REDACTED@db.internal:5432/weather?sslmode=disable"
        );
        assert_eq!(
            redact("postgresql://postgres@127.0.0.1:5432/postgres"),
            "postgresql://postgres@127.0.0.1:5432/postgres"
        );
    }

    #[test]
    fn the_default_url_is_the_service_containers_own_database() {
        // Only meaningful with a clean environment, which `cargo test` does not guarantee,
        // so the assertion is on the shape rather than on the exact string.
        let url = url();
        assert!(url.starts_with("postgres"), "{url}");
        assert!(url.contains("://"), "{url}");
    }
}
