//! The metric-registry drift gate.
//!
//! DESIGN.md R-2: generated output is committed, and CI *regenerates and compares* rather
//! than rebuilding. `--check` therefore generates into `target/codegen-check/` and diffs.
//! It must never write into the working tree — a check that fixes the problem it is
//! checking for always passes.
//!
//! The three paths below are the whole contract between W8 and W2:
//!
//! | | |
//! |---|---|
//! | registry      | `metrics/registry.toml` |
//! | generator     | `cargo run -p mw-codegen -- --registry <file> --out <dir>` |
//! | committed out | `generated/` |
//!
//! W2 may rename any of them; this file is the one place that has to change with it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

const REGISTRY: &str = "metrics/registry.toml";
const GENERATOR_PACKAGE: &str = "mw-codegen";
const COMMITTED_OUTPUT: &str = "generated";
const CHECK_OUTPUT: &str = "target/codegen-check";

pub fn main(args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    let root = crate::repo_root();
    let registry = root.join(REGISTRY);

    if !registry.exists() {
        return Err(format!(
            "{REGISTRY} does not exist.\n  \
             The metric registry is W2's deliverable. Until it lands, the drift gate is \
             deferred — run `cargo xtask gate codegen-drift`, which reads ci/gates.toml \
             and reports that rather than inventing a result."
        ));
    }

    let out = if check {
        let out = root.join(CHECK_OUTPUT);
        if out.exists() {
            std::fs::remove_dir_all(&out)
                .map_err(|e| format!("clearing {}: {e}", out.display()))?;
        }
        out
    } else {
        root.join(COMMITTED_OUTPUT)
    };
    std::fs::create_dir_all(&out).map_err(|e| format!("creating {}: {e}", out.display()))?;

    let status = Command::new(cargo())
        .current_dir(&root)
        .args([
            "run",
            "--quiet",
            "--package",
            GENERATOR_PACKAGE,
            "--",
            "--registry",
        ])
        .arg(&registry)
        .arg("--out")
        .arg(&out)
        .status()
        .map_err(|e| format!("running the generator: {e}"))?;
    if !status.success() {
        return Err(format!("the generator exited with {status}"));
    }

    if !check {
        println!("generated into {}", out.display());
        return Ok(());
    }

    let committed = root.join(COMMITTED_OUTPUT);
    let diffs = compare(&committed, &out)?;
    if diffs.is_empty() {
        println!("no drift: {COMMITTED_OUTPUT}/ matches what {REGISTRY} generates");
        return Ok(());
    }

    eprintln!("generated output has drifted from {REGISTRY}:");
    for d in &diffs {
        eprintln!("  {d}");
    }
    show_diff(&committed, &out);
    Err(format!(
        "{} generated file(s) differ. Regenerate with `cargo xtask codegen` and commit \
         the result; do not edit {COMMITTED_OUTPUT}/ by hand.",
        diffs.len()
    ))
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

#[derive(Debug)]
enum Difference {
    Missing(PathBuf),
    Unexpected(PathBuf),
    Changed(PathBuf),
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(p) => write!(f, "missing from the tree: {}", p.display()),
            Self::Unexpected(p) => {
                write!(f, "in the tree but no longer generated: {}", p.display())
            }
            Self::Changed(p) => write!(f, "contents differ: {}", p.display()),
        }
    }
}

fn compare(committed: &Path, fresh: &Path) -> Result<Vec<Difference>, String> {
    let a = relative_files(committed)?;
    let b = relative_files(fresh)?;
    let mut out = Vec::new();

    for p in b.difference(&a) {
        out.push(Difference::Missing(p.clone()));
    }
    for p in a.difference(&b) {
        out.push(Difference::Unexpected(p.clone()));
    }
    for p in a.intersection(&b) {
        let left = std::fs::read(committed.join(p)).map_err(|e| format!("{}: {e}", p.display()))?;
        let right = std::fs::read(fresh.join(p)).map_err(|e| format!("{}: {e}", p.display()))?;
        if left != right {
            out.push(Difference::Changed(p.clone()));
        }
    }
    out.sort_by_key(|d| format!("{d}"));
    Ok(out)
}

fn relative_files(root: &Path) -> Result<BTreeSet<PathBuf>, String> {
    let mut acc = BTreeSet::new();
    if !root.exists() {
        return Ok(acc);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("reading {}: {e}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                acc.insert(rel.to_path_buf());
            }
        }
    }
    Ok(acc)
}

/// Best effort: a unified diff is far more useful in a CI log than a list of filenames,
/// but the gate's verdict does not depend on `diff` being installed.
fn show_diff(committed: &Path, fresh: &Path) {
    let _ = Command::new("diff")
        .args(["-ru", "--"])
        .arg(committed)
        .arg(fresh)
        .status();
}
