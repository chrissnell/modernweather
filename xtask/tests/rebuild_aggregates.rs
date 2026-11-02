//! `ci/db/rebuild-aggregates.sh`, driven without a database.
//!
//! An integration test rather than a unit test because the subject is a shell script, and
//! it is testable at all because the script already takes `PSQL` as the whole psql
//! invocation (W9's edit, so the script works through a bastion or `kubectl exec`).
//! `PSQL=echo` turns every statement it would run into a line on stdout, so the plan the
//! script executes can be asserted against without a TimescaleDB anywhere.
//!
//! What is under test is N-16: phase 1 refreshes each level over the *widest* band any
//! level above it serves rather than over its own, and `--recent-only` names the levels it
//! left incomplete. Both were silent failures — the first wrote holes into
//! `observations_1d` and kept them, the second said "older ranges are NOT materialised"
//! without saying which of the four levels that meant.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent")
        .to_path_buf()
}

/// Runs the script with `PSQL=echo`, returning (stdout, stderr).
fn run(args: &[&str]) -> (String, String) {
    let root = repo_root();
    let out = Command::new("bash")
        .arg(root.join("ci/db/rebuild-aggregates.sh"))
        .args(args)
        .env("PSQL", "echo")
        .env_remove("DATABASE_URL")
        .current_dir(&root)
        .output()
        .expect("bash runs the script");
    assert!(
        out.status.success(),
        "the script failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8(out.stdout).expect("utf-8"),
        String::from_utf8(out.stderr).expect("utf-8"),
    )
}

const LEVELS: [&str; 4] = [
    "observations_1m",
    "observations_5m",
    "observations_1h",
    "observations_1d",
];

/// The defect itself. Every level, the minute level included, is refreshed over the daily
/// level's band, because the daily level reads the hourly level, which reads the 5-minute
/// level, which reads the minute level — so a correct 366-day daily bucket needs all four
/// materialised over 366 days.
#[test]
fn phase_one_refreshes_every_level_over_the_widest_band_above_it() {
    let (stdout, _) = run(&["--recent-only"]);
    for level in LEVELS {
        let want = format!(
            "CALL refresh_continuous_aggregate('{level}', now() - INTERVAL '366 days', NULL)"
        );
        assert!(stdout.contains(&want), "missing `{want}`\n{stdout}");
    }
}

/// The negative half, and the one that would have passed before N-16: no level is
/// refreshed over its own narrow band any more. These three intervals are exactly what
/// phase 1 used to pass for the lower three levels, and each of them is a hole the level
/// above would have aggregated and cached.
#[test]
fn phase_one_no_longer_refreshes_a_level_over_its_own_narrow_band() {
    let (stdout, _) = run(&["--recent-only"]);
    for narrow in ["6 hours", "48 hours", "14 days"] {
        assert!(
            !stdout.contains(&format!("INTERVAL '{narrow}'")),
            "phase 1 still uses the {narrow} band, which the level above reads past\n{stdout}"
        );
    }
}

/// Bottom-up is required, not merely preferable: a range materialised top-down has the
/// upper level reading a hole and caching it. Widening the bands must not have disturbed
/// the order.
#[test]
fn the_refresh_order_is_still_bottom_up() {
    let (stdout, _) = run(&["--recent-only"]);
    let mut at = 0;
    for level in LEVELS {
        let needle = format!("refresh_continuous_aggregate('{level}'");
        let found = stdout[at..].find(&needle).unwrap_or_else(|| {
            panic!("{level} is not refreshed after the level below it\n{stdout}")
        });
        at += found + needle.len();
    }
}

/// `--recent-only` is what docs/database-schema.md §4 recommends after a metric addition,
/// and it stops before phase 2. It now says which levels that leaves short, by name.
/// Exactly one level is short: `observations_1d` is the only one whose retention is
/// unbounded, and the other three retain 7, 14 and 90 days, all inside phase 1's band.
#[test]
fn recent_only_names_the_levels_it_left_incomplete() {
    let (stdout, stderr) = run(&["--recent-only"]);
    assert!(
        stderr.contains("These levels hold history older than phase 1 refreshed them over"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("observations_1d: materialised over the last 366 days"),
        "{stderr}"
    );
    assert!(
        stderr.contains("retention is unbounded"),
        "the warning has to say why, or it is just a different kind of silence:\n{stderr}"
    );
    for complete in ["observations_1m", "observations_5m", "observations_1h"] {
        assert!(
            !stderr.contains(complete),
            "{complete} retains less history than phase 1 materialised, so naming it \
             would train the operator to ignore this warning:\n{stderr}"
        );
    }
    assert!(stderr.contains("Re-run without --recent-only"), "{stderr}");
    // And it must not overstate what is broken. `/span` cannot read a daily bucket older
    // than its own one-year maximum and phase 1 now covers 366 days, so every band /span
    // serves IS correct here. A warning that claims otherwise is false on every run, which
    // is the same cry-wolf argument the loop above makes for the other three levels.
    assert!(
        !stderr.contains("/span read of them will be short"),
        "the warning must not claim a /span band is short when none is:\n{stderr}"
    );
    assert!(
        stderr.contains("Every band /span serves is correct"),
        "{stderr}"
    );
}

/// A full run does phase 1 and then phase 2 over unbounded history, and says nothing about
/// incompleteness because there is none. The warning belongs to `--recent-only` alone.
#[test]
fn a_full_run_backfills_unbounded_history_and_warns_about_nothing() {
    let (stdout, stderr) = run(&[]);
    for level in LEVELS {
        assert!(
            stdout.contains(&format!(
                "CALL refresh_continuous_aggregate('{level}', NULL, NULL)"
            )),
            "phase 2 skipped {level}\n{stdout}"
        );
    }
    assert!(
        !stderr.contains("hold history older than"),
        "a full run leaves nothing short:\n{stderr}"
    );
}

/// D-17's relaxed commit durability is required rather than an optimisation — 306 s against
/// 25.6 s on the measured rebuild — and it is a property of these sessions only. Nothing
/// else asserts it, so deleting it from `refresh()` would cost a twelvefold slowdown with
/// no error anywhere.
#[test]
fn every_refresh_session_relaxes_commit_durability() {
    for args in [
        vec!["--recent-only"],
        vec![],
        vec!["--from", "2026-01-01", "--to", "2026-02-01"],
    ] {
        let (stdout, _) = run(&args);
        let refreshes = stdout.matches("refresh_continuous_aggregate(").count();
        let relaxed = stdout.matches("SET synchronous_commit = off").count();
        assert!(refreshes > 0, "no refresh at all for {args:?}\n{stdout}");
        assert_eq!(
            relaxed, refreshes,
            "every refresh session must set it, not just the first ({args:?})\n{stdout}"
        );
    }
}

/// A bounded `--from`/`--to` range is the operator's own window and is not phase 1: it
/// must stay exactly what was asked for, bottom-up, with no widening.
#[test]
fn an_explicit_range_is_not_widened() {
    let (stdout, _) = run(&["--from", "2026-01-01", "--to", "2026-02-01"]);
    for level in LEVELS {
        assert!(
            stdout.contains(&format!(
                "CALL refresh_continuous_aggregate('{level}', '2026-01-01', '2026-02-01')"
            )),
            "{level} did not get the range it was given\n{stdout}"
        );
    }
    assert!(!stdout.contains("366 days"), "{stdout}");
}
