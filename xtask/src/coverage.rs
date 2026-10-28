//! Coverage as a ratchet, not a threshold.
//!
//! `--check` fails if measured line coverage is below the committed floor. `--bump` raises
//! the floor to the measured value and is run only on the default branch. The floor can
//! therefore rise and never fall, which is the property a fixed percentage target does not
//! have: a target invites tests written to reach a number, while a ratchet only forbids
//! going backwards.
//!
//! A small tolerance absorbs the fact that coverage is not perfectly deterministic across
//! runs (inlining and generic instantiation move line counts a little).

use std::process::Command;

const FLOOR_FILE: &str = "ci/coverage-floor.txt";
const TOLERANCE_PCT: f64 = 0.5;

pub fn main(args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    let bump = args.iter().any(|a| a == "--bump");
    if check == bump {
        return Err("pass exactly one of --check or --bump".to_string());
    }

    let measured = measure()?;
    let floor = read_floor()?;
    println!("line coverage {measured:.2}% (floor {floor:.2}%)");

    if bump {
        if measured > floor {
            std::fs::write(
                crate::repo_root().join(FLOOR_FILE),
                format!("{measured:.2}\n"),
            )
            .map_err(|e| format!("writing {FLOOR_FILE}: {e}"))?;
            println!("floor raised to {measured:.2}%");
        } else {
            println!("floor unchanged");
        }
        return Ok(());
    }

    if measured + TOLERANCE_PCT < floor {
        return Err(format!(
            "coverage fell from {floor:.2}% to {measured:.2}% (tolerance {TOLERANCE_PCT}%).\n  \
             The ratchet only moves up. Either cover the new code, or — if the drop is \
             deliberate because tested code was deleted — lower {FLOOR_FILE} in the same \
             commit, which makes it a reviewed decision instead of a silent one."
        ));
    }
    Ok(())
}

fn read_floor() -> Result<f64, String> {
    let path = crate::repo_root().join(FLOOR_FILE);
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    text.trim()
        .parse()
        .map_err(|e| format!("{} does not contain a number: {e}", path.display()))
}

/// `cargo llvm-cov --summary-only --json` over the workspace. An empty workspace reports
/// zero lines, which is 0.00% rather than an error — that is the correct reading on a repo
/// that has no code yet, and it keeps the gate honest the moment code arrives.
fn measure() -> Result<f64, String> {
    let out = Command::new(cargo())
        .current_dir(crate::repo_root())
        .args([
            "llvm-cov",
            "--workspace",
            "--all-features",
            "--summary-only",
            "--json",
        ])
        .output()
        .map_err(|e| {
            format!(
                "running cargo llvm-cov: {e}\n  \
                 Install it with `cargo install cargo-llvm-cov` (CI uses \
                 taiki-e/install-action)."
            )
        })?;
    if !out.status.success() {
        return Err(format!(
            "cargo llvm-cov exited with {}:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let json = String::from_utf8_lossy(&out.stdout);
    percent_from_summary(&json)
}

/// Pulled out of `measure` so the parsing is testable without llvm-cov installed.
///
/// The shape is llvm-cov's export format: `data[0].totals.lines.{count,covered,percent}`.
/// `percent` is absent or NaN when `count` is zero, so `covered`/`count` is the value that
/// is always meaningful.
///
/// `"totals"` has to be located first. Even with `--summary-only`, the output carries a
/// per-file `files[]` array *before* `totals`, each entry with its own `summary.lines` —
/// so searching for the first `"lines"` silently measures one arbitrary file instead of
/// the workspace. The test below pins that down.
fn percent_from_summary(json: &str) -> Result<f64, String> {
    let totals_at = json
        .find("\"totals\"")
        .ok_or_else(|| "llvm-cov output has no \"totals\" object".to_string())?;
    let after_totals = &json[totals_at..];
    let lines_at = after_totals
        .find("\"lines\"")
        .ok_or_else(|| "llvm-cov totals have no \"lines\" entry".to_string())?;
    let tail = &after_totals[lines_at..];
    let count = scan_number(tail, "\"count\":")?;
    let covered = scan_number(tail, "\"covered\":")?;
    if count == 0.0 {
        return Ok(0.0);
    }
    Ok(covered / count * 100.0)
}

fn scan_number(haystack: &str, key: &str) -> Result<f64, String> {
    let at = haystack
        .find(key)
        .ok_or_else(|| format!("llvm-cov output has no {key}"))?;
    let rest = haystack[at + key.len()..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == 'e' || c == '+'))
        .unwrap_or(rest.len());
    rest[..end]
        .parse()
        .map_err(|e| format!("{key} is not a number: {e}"))
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

#[cfg(test)]
mod tests {
    use super::percent_from_summary;

    #[test]
    fn empty_workspace_reads_as_zero() {
        let json = r#"{"data":[{"totals":{"lines":{"count":0,"covered":0,"percent":0}}}]}"#;
        assert_eq!(percent_from_summary(json).unwrap(), 0.0);
    }

    #[test]
    fn percent_comes_from_covered_over_count() {
        // Deliberately disagrees with the "percent" field: covered/count is the value
        // that is meaningful in every case, so that is the one used.
        let json = r#"{"data":[{"totals":{"lines":{"count":200,"covered":150,"percent":99.9}}}]}"#;
        assert_eq!(percent_from_summary(json).unwrap(), 75.0);
    }

    #[test]
    fn per_file_summaries_before_totals_are_not_mistaken_for_totals() {
        // This is the real shape of `cargo llvm-cov --summary-only --json`: a files[]
        // array, each entry with its own summary.lines, and totals at the end. Searching
        // for the first "lines" reports one file's coverage as the workspace's.
        let json = r#"{"data":[{
            "files":[
              {"filename":"a.rs","summary":{"lines":{"count":128,"covered":0,"percent":0.0}}},
              {"filename":"b.rs","summary":{"lines":{"count":10,"covered":10,"percent":100.0}}}
            ],
            "totals":{"lines":{"count":138,"covered":10,"percent":7.24}}
        }]}"#;
        let got = percent_from_summary(json).unwrap();
        assert!((got - 10.0 / 138.0 * 100.0).abs() < 1e-9, "got {got}");
    }

    #[test]
    fn missing_totals_is_an_error_not_a_zero() {
        assert!(percent_from_summary("{}").is_err());
    }
}
