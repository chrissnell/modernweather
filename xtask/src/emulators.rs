//! `cargo xtask emulator-tests` — the `driver-emulators` gate's command.
//!
//! Reads `tests/emulators/manifest.toml` and runs each declared emulator's test target,
//! then **asserts that exactly the declared number of tests passed**. That assertion is
//! the whole point: the tests are `#[ignore]`d because they need a Go toolchain, a filter
//! that matches nothing exits zero, and a gate that reports green while running nothing is
//! `docs/reconciliation.md` G-1 — the failure mode this gate was armed to avoid being
//! another instance of.
//!
//! Three other things it checks, each because the manifest would otherwise be able to lie:
//!
//! - the reference commit is a **full object name**, since "pinned" and "pinned to a
//!   branch name" are different things (A-W8-4);
//! - every emulator's `path` exists, so an entry cannot outlive the directory it names;
//! - there is at least one runnable emulator, so emptying the `[[emulator]]` list is not a
//!   way to make the gate pass.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use serde::Deserialize;

const MANIFEST: &str = "tests/emulators/manifest.toml";

#[derive(Debug, Deserialize)]
struct Manifest {
    reference: Reference,
    #[serde(default, rename = "emulator")]
    emulators: Vec<Emulator>,
    #[serde(default, rename = "pending")]
    pending: Vec<Pending>,
}

#[derive(Debug, Deserialize)]
struct Reference {
    repo: String,
    commit: String,
    #[serde(default)]
    committed: String,
}

#[derive(Debug, Deserialize)]
struct Emulator {
    id: String,
    emulates: String,
    driver: String,
    /// `"reference"` for an emulator checked out of the pinned reference repository, or
    /// `"this-repository"` for one that lives here. The distinction is D-40's independent
    /// authorship, and it is reported on every run rather than buried in the manifest.
    origin: String,
    path: String,
    package: String,
    test_target: String,
    /// Every test this emulator's target must run, by name.
    ///
    /// **The count alone was not enough, and the manifest used to claim it was.** A count
    /// catches a removed test and a filter that matched nothing; it does not catch a
    /// *rename*, and it does not catch four tests whose bodies were emptied — which is the
    /// realistic path, because the way a flaky emulator test gets "fixed" is by gutting it.
    /// Demonstrated rather than reasoned about: four empty `#[ignore]`d functions produced
    /// `ok: 4 test(s) passed` and exit 0 with no emulator process ever started. Names bind
    /// identity, so now they have to be the same tests.
    test_names: Vec<String>,
    #[serde(default)]
    note: String,
}

impl Emulator {
    /// How many tests this emulator declares.
    fn tests(&self) -> usize {
        self.test_names.len()
    }
}

#[derive(Debug, Deserialize)]
struct Pending {
    id: String,
    emulates: String,
    #[serde(default)]
    reference_path: String,
    driver: String,
    #[serde(default)]
    note: String,
}

pub fn main(args: &[String]) -> Result<ExitCode, String> {
    if let Some(unexpected) = args.first() {
        return Err(format!(
            "emulator-tests takes no arguments, got `{unexpected}`. It reads {MANIFEST} \
             and the tree, and has no modes."
        ));
    }
    let root = crate::repo_root();
    let manifest = load(&root)?;

    println!(
        "emulators: reference {} at {} ({})",
        manifest.reference.repo,
        &manifest.reference.commit[..12],
        if manifest.reference.committed.is_empty() {
            "undated"
        } else {
            &manifest.reference.committed
        }
    );
    for pending in &manifest.pending {
        println!(
            "  PENDING  {:<16} {} -- needs {}",
            pending.id, pending.emulates, pending.driver
        );
        if !pending.reference_path.is_empty() {
            println!("           reference: {}", pending.reference_path);
        }
        if !pending.note.is_empty() {
            // The note is why it is pending, and a pending entry whose reason is not
            // printed is a list nobody reads.
            println!("           {}", first_line(&pending.note));
        }
    }

    let mut failures = Vec::new();
    for emulator in &manifest.emulators {
        println!(
            "  RUN      {:<16} {} -- driving {} ({})",
            emulator.id, emulator.emulates, emulator.driver, emulator.origin
        );
        if emulator.origin != "reference" {
            // D-40's rationale is that the emulator is not written by whoever wrote the
            // driver. An emulator in this repository does not have that property, and
            // saying so on every run is the difference between a recorded limitation and a
            // forgotten one.
            println!(
                "           NOTE: this emulator lives in THIS repository, so it is not \
                 the independently-written oracle D-40 asks for. See {MANIFEST}."
            );
        }
        if !emulator.note.is_empty() {
            println!("           {}", first_line(&emulator.note));
        }
        match run(&root, emulator) {
            Ok(()) => println!("           ok: {} test(s) passed", emulator.tests()),
            Err(why) => {
                println!("           FAILED: {why}");
                failures.push(format!("{}: {why}", emulator.id));
            }
        }
    }

    if failures.is_empty() {
        println!(
            "emulator-tests: {} emulator(s), {} test(s), {} pending",
            manifest.emulators.len(),
            manifest
                .emulators
                .iter()
                .map(Emulator::tests)
                .sum::<usize>(),
            manifest.pending.len()
        );
        Ok(ExitCode::SUCCESS)
    } else {
        for failure in &failures {
            eprintln!("emulator-tests: {failure}");
        }
        Ok(ExitCode::FAILURE)
    }
}

fn load(root: &Path) -> Result<Manifest, String> {
    let path = root.join(MANIFEST);
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let manifest: Manifest =
        toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    check(root, &manifest).map_err(|e| format!("{MANIFEST}: {e}"))?;
    Ok(manifest)
}

/// Everything about the manifest that can be wrong without running a test.
///
/// Separate from [`main`] so that the test below can exercise it, and so that a manifest
/// problem fails the **load** rather than surfacing as a test that did not run.
fn check(root: &Path, manifest: &Manifest) -> Result<(), String> {
    let commit = &manifest.reference.commit;
    if commit.len() != 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "`reference.commit` is `{commit}`, which is not a full 40-character object \
             name. A-W8-4 requires the emulators to be consumed at a PINNED commit, and a \
             branch name or an abbreviation is not one -- the reference repository is not \
             frozen, so an unrelated upstream commit would otherwise turn this gate red."
        ));
    }
    if manifest.reference.repo.is_empty() {
        return Err("`reference.repo` is empty".to_string());
    }
    if manifest.emulators.is_empty() {
        return Err(
            "there are no `[[emulator]]` entries, so this gate would run nothing and pass. \
             That is docs/reconciliation.md G-1, which is the failure this gate exists \
             against: an emulator whose driver has not landed belongs in `[[pending]]`, \
             which is printed and not run."
                .to_string(),
        );
    }
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    for emulator in &manifest.emulators {
        if seen.insert(emulator.id.as_str(), ()).is_some() {
            return Err(format!("two emulators share the id `{}`", emulator.id));
        }
        if emulator.test_names.is_empty() {
            return Err(format!(
                "emulator `{}` declares no tests. The declared set is what makes \"the gate \
                 ran nothing\" a failure, so empty is the one value it may not be.",
                emulator.id
            ));
        }
        let mut names: Vec<&str> = emulator.test_names.iter().map(String::as_str).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        if names.len() != before {
            return Err(format!(
                "emulator `{}` declares the same test twice, so the count and the set \
                 disagree about how many there are",
                emulator.id
            ));
        }
        let path = root.join(&emulator.path);
        if !path.exists() {
            return Err(format!(
                "emulator `{}` names `{}`, which does not exist",
                emulator.id, emulator.path
            ));
        }
    }
    for pending in &manifest.pending {
        if seen.contains_key(pending.id.as_str()) {
            return Err(format!(
                "`{}` is both runnable and pending; it is one or the other",
                pending.id
            ));
        }
        if pending.driver.is_empty() {
            return Err(format!(
                "pending emulator `{}` names no work item. A deferred check with no owner \
                 is docs/reconciliation.md G-5.",
                pending.id
            ));
        }
    }
    Ok(())
}

/// Run one emulator's tests and assert the declared number passed.
fn run(root: &Path, emulator: &Emulator) -> Result<(), String> {
    let output = Command::new(cargo())
        .current_dir(root)
        .args([
            "test",
            "--locked",
            "-p",
            &emulator.package,
            "--test",
            &emulator.test_target,
            "--",
            "--ignored",
            // One at a time: each test starts an emulator that binds sockets, and a
            // failure that only happens under contention is a worse signal than a slower
            // gate.
            "--test-threads=1",
        ])
        // Captured rather than inherited, because the summary line is what is asserted.
        // Printed below either way, so a failure is readable in the job log.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("running cargo test: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let summary = summary(&stdout);
    let report = |why: String| -> String {
        // The whole output, once, when something is wrong. A gate that failed without
        // showing what the tests said is a gate somebody has to reproduce locally to read.
        println!("--- cargo test stdout ---\n{stdout}");
        println!("--- cargo test stderr ---\n{stderr}");
        why
    };

    if !output.status.success() {
        return Err(report(format!(
            "cargo test exited {}",
            output
                .status
                .code()
                .map_or_else(|| "by signal".to_string(), |c| c.to_string())
        )));
    }
    let Some((passed, failed)) = summary else {
        return Err(report(
            "cargo test printed no `test result:` line, so nothing can be said about what \
             it ran"
                .to_string(),
        ));
    };
    if failed != 0 {
        return Err(report(format!("{failed} test(s) failed")));
    }
    if passed != emulator.tests() {
        return Err(report(format!(
            "{passed} test(s) passed and the manifest declares {}. Either a test was \
             removed, or the filter matched nothing -- and a filter that matches nothing \
             exits zero, which is why the declared set exists.",
            emulator.tests()
        )));
    }
    // The count says how many ran; the names say WHICH. Without this a rename passes, and
    // so do four tests whose bodies were emptied -- verified, which is why this is here.
    let ran = passed_names(&stdout);
    let missing: Vec<&str> = emulator
        .test_names
        .iter()
        .map(String::as_str)
        .filter(|name| !ran.iter().any(|r| r == name))
        .collect();
    if !missing.is_empty() {
        return Err(report(format!(
            "the manifest declares {} test(s) that did not pass: {}. A test that was \
             renamed or replaced is a test this gate no longer knows it is running, which \
             is what the declared set is for -- rename it in {MANIFEST} in the same change.",
            missing.len(),
            missing.join(", ")
        )));
    }
    Ok(())
}

/// Every test libtest reported as passing.
///
/// `test the_broadcast_driver_arms_the_unit_and_keeps_receiving ... ok`
///
/// The same trade as [`summary`]: this command chose the invocation, and the alternative --
/// trusting a number -- cannot tell four known tests from four unknown ones.
fn passed_names(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("test ")?;
            let (name, verdict) = rest.rsplit_once(" ... ")?;
            (verdict.trim() == "ok").then_some(name.trim())
        })
        .collect()
}

/// The `passed` and `failed` counts from libtest's summary line.
///
/// `test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; ...`
///
/// Parsing output is usually a mistake and is the right answer here: this command chose the
/// invocation, the format is libtest's stable summary, and the alternative — trusting the
/// exit status — is exactly what cannot distinguish "four passed" from "nothing ran".
fn summary(stdout: &str) -> Option<(usize, usize)> {
    let line = stdout.lines().find(|l| l.starts_with("test result:"))?;
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
    // The cargo that invoked this xtask, so a toolchain override is inherited rather than
    // resolved again from PATH.
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

fn first_line(text: &str) -> &str {
    text.trim().lines().next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        load(&crate::repo_root()).expect("the committed manifest loads")
    }

    #[test]
    fn the_committed_manifest_is_valid() {
        let m = manifest();
        assert!(
            !m.emulators.is_empty(),
            "the gate would run nothing and pass"
        );
        assert_eq!(m.reference.commit.len(), 40);
        // Every runnable entry names a real test target in a real package.
        for emulator in &m.emulators {
            let target = crate::repo_root()
                .join("crates")
                .join(&emulator.package)
                .join("tests")
                .join(format!("{}.rs", emulator.test_target));
            assert!(
                target.exists(),
                "emulator `{}` names {}, which does not exist",
                emulator.id,
                target.display()
            );
            assert!(emulator.tests() > 0);
            assert!(!emulator.emulates.is_empty());
            assert!(!emulator.driver.is_empty());
        }
    }

    /// The property the whole command exists for: an empty list is a failure, not a pass.
    #[test]
    fn an_empty_manifest_is_refused() {
        let empty: Manifest = toml::from_str(
            r#"
            [reference]
            repo = "git@example.com:x.git"
            commit = "c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf"
            "#,
        )
        .expect("parses");
        let e = check(&crate::repo_root(), &empty).expect_err("refused");
        assert!(e.contains("G-1"), "{e}");
    }

    #[test]
    fn an_unpinned_reference_is_refused() {
        for commit in ["main", "c3ec9f7", ""] {
            let m: Manifest = toml::from_str(&format!(
                r#"
                [reference]
                repo = "git@example.com:x.git"
                commit = "{commit}"
                [[emulator]]
                id = "x"
                emulates = "x"
                driver = "x"
                origin = "this-repository"
                path = "tests/emulators/wll"
                package = "mw-station"
                test_target = "emulator"
                test_names = ["x"]
                "#
            ))
            .expect("parses");
            let e = check(&crate::repo_root(), &m).expect_err("refused");
            assert!(e.contains("A-W8-4"), "`{commit}`: {e}");
        }
    }

    #[test]
    fn an_empty_test_set_a_duplicate_name_and_a_missing_path_are_all_refused() {
        let with = |names: &str, path: &str| -> Manifest {
            toml::from_str(&format!(
                r#"
                [reference]
                repo = "git@example.com:x.git"
                commit = "c3ec9f7d7dec2c5f71a1f48b8af2b9b0921b7bcf"
                [[emulator]]
                id = "x"
                emulates = "x"
                driver = "x"
                origin = "reference"
                path = "{path}"
                package = "mw-station"
                test_target = "emulator"
                test_names = {names}
                "#
            ))
            .expect("parses")
        };
        let root = crate::repo_root();
        assert!(check(&root, &with("[]", "tests/emulators/wll"))
            .expect_err("refused")
            .contains("declares no tests"));
        assert!(check(&root, &with(r#"["a", "a"]"#, "tests/emulators/wll"))
            .expect_err("refused")
            .contains("the same test twice"));
        assert!(check(&root, &with(r#"["a"]"#, "tests/emulators/nowhere"))
            .expect_err("refused")
            .contains("does not exist"));
    }

    /// The hole the declared set exists to close: four tests with the right *shape* and the
    /// wrong *identity*. The count cannot see this, which is why it is not the only check.
    #[test]
    fn the_names_libtest_reports_are_read_the_way_libtest_writes_them() {
        let stdout = "running 2 tests\n\
                      test the_broadcast_driver_arms_the_unit_and_keeps_receiving ... ok\n\
                      test a_refusal_carrying_two_hundred_fails_the_re_arm ... ok\n\
                      test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; \
                      0 filtered out; finished in 3.85s\n";
        assert_eq!(
            passed_names(stdout),
            vec![
                "the_broadcast_driver_arms_the_unit_and_keeps_receiving",
                "a_refusal_carrying_two_hundred_fails_the_re_arm",
            ]
        );
        // An ignored test is not a passing one, and neither is a failing one.
        let mixed = "test a ... ok\n\
                     test b ... ignored, needs a Go toolchain\n\
                     test c ... FAILED\n";
        assert_eq!(passed_names(mixed), vec!["a"]);
        assert!(passed_names("the test binary did not start").is_empty());
    }

    /// Every test the committed manifest declares exists in the file it names.
    ///
    /// The runner checks the other direction at gate time — that each declared name
    /// actually passed — and this checks it at `cargo test` time, where nobody needs a Go
    /// toolchain to find out that a rename was only half done.
    #[test]
    fn every_declared_test_name_exists_in_its_target() {
        for emulator in &manifest().emulators {
            let path = crate::repo_root()
                .join("crates")
                .join(&emulator.package)
                .join("tests")
                .join(format!("{}.rs", emulator.test_target));
            let source = std::fs::read_to_string(&path).expect("the target is readable");
            for name in &emulator.test_names {
                assert!(
                    source.contains(&format!("fn {name}(")),
                    "`{name}` is declared in {MANIFEST} and is not in {}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn the_summary_line_is_read_the_way_libtest_writes_it() {
        assert_eq!(
            summary(
                "running 4 tests\ntest result: ok. 4 passed; 0 failed; 0 ignored; \
                 0 measured; 0 filtered out; finished in 3.85s\n"
            ),
            Some((4, 0))
        );
        assert_eq!(
            summary(
                "test result: FAILED. 1 passed; 3 failed; 0 ignored; 0 measured; \
                 0 filtered out; finished in 1.00s"
            ),
            Some((1, 3))
        );
        // The case the count exists for: the filter matched nothing and the status is zero.
        assert_eq!(
            summary(
                "test result: ok. 0 passed; 0 failed; 4 ignored; 0 measured; \
                 0 filtered out; finished in 0.00s"
            ),
            Some((0, 0))
        );
        assert_eq!(summary("the test binary did not start"), None);
    }
}
