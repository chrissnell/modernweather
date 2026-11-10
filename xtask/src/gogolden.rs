//! `cargo xtask go-golden` -- regenerate the two tables that are captures of Go's own
//! behaviour.
//!
//! Two of `crates/mw-web`'s tests assert against output produced by **running** Go rather
//! than against a reading of it, which is how M4-2 pinned the enrich formulae and the only
//! way a claim like "this reproduces `html/template`" can be checked instead of believed:
//!
//! | Golden | Asserted by | Produced by |
//! |---|---|---|
//! | `crates/mw-web/tests/golden/go-escapes.tsv` | `crates/mw-web/tests/escaping.rs` | `ci/go-golden/escapes` |
//! | `crates/mw-web/tests/golden/go-content-types.tsv` | `crates/mw-web/tests/mime.rs` | `ci/go-golden/mime` |
//!
//! **This command exists because M4-3's code review found both goldens produced by programs
//! nobody who checked out the branch could run.** They lived under `scratch/`, which is in
//! `.gitignore`, and one of them hard-coded an absolute path containing the runtime's working
//! directory -- the opposite of the argument the tests rest on. The programs are now
//! committed under `ci/go-golden/`, resolve their inputs relative to the repository root, and
//! are run from here so that the working directory is right by construction.
//!
//! **Not a gate, and deliberately.** Re-deriving these on every PR would need a Go toolchain
//! in a job whose whole point is that there is no Go in the Rust build, and a drift check
//! would turn a Go point release that changed a byte into a red PR on a commit that touched
//! nothing. The tables are reviewed artefacts: a regeneration is a diff somebody reads, and
//! if it is not empty then either Go's behaviour or the asset tree changed, and the question
//! is which.

use std::process::{Command, ExitCode};

/// One generator: where its program is, and where its output belongs.
const GENERATORS: [(&str, &str); 2] = [
    ("escapes", "crates/mw-web/tests/golden/go-escapes.tsv"),
    ("mime", "crates/mw-web/tests/golden/go-content-types.tsv"),
];

/// The header every table carries, so that a reader who finds one knows how to re-derive it.
///
/// Written here rather than by the Go programs because it names the command, and the command
/// is this file.
fn header(name: &str) -> String {
    format!(
        "# Produced by `cargo xtask go-golden` from ci/go-golden/{name}, which runs Go's own\n\
         # html/template or net/http over this tree. Do not edit: a change here is a change\n\
         # in Go's behaviour or in the asset tree, and the diff is the review.\n"
    )
}

pub fn main(args: &[String]) -> Result<ExitCode, String> {
    if let Some(bad) = args.iter().find(|a| a.starts_with('-')) {
        return Err(format!(
            "`go-golden` takes no flags, got `{bad}`. It regenerates every table; \
             regenerating one is `cd ci/go-golden/<name> && go run .` from the repository root."
        ));
    }
    let root = crate::repo_root();
    which_go()?;

    for (name, out) in GENERATORS {
        let dir = root.join("ci/go-golden").join(name);
        if !dir.is_dir() {
            return Err(format!("{} does not exist", dir.display()));
        }
        println!("==> ci/go-golden/{name} -> {out}");
        // **`current_dir` is the generator's own directory, because each carries its own
        // `go.mod`** -- `go run ./ci/go-golden/<name>` from the root fails with "cannot find
        // main module". The repository root travels in `MW_ROOT` instead, which is what
        // replaced the absolute path M4-3's code review found hard-coded in one of them.
        let produced = Command::new("go")
            .args(["run", "."])
            .current_dir(&dir)
            .env("MW_ROOT", &root)
            .output()
            .map_err(|e| format!("running ci/go-golden/{name}: {e}"))?;
        if !produced.status.success() {
            return Err(format!(
                "ci/go-golden/{name} failed ({}):\n{}",
                produced.status,
                String::from_utf8_lossy(&produced.stderr)
            ));
        }
        let mut text = header(name);
        text.push_str(&String::from_utf8_lossy(&produced.stdout));
        let path = root.join(out);
        let previous = std::fs::read_to_string(&path).unwrap_or_default();
        std::fs::write(&path, &text).map_err(|e| format!("writing {}: {e}", path.display()))?;
        let rows = text.lines().filter(|l| !l.starts_with('#')).count();
        if previous == text {
            println!("    {rows} row(s), unchanged");
        } else {
            println!("    {rows} row(s), CHANGED -- read the diff before committing it");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A Go toolchain, or a message that says what to install.
///
/// A refusal rather than a skip, for `cargo xtask db-tests`'s reason: a command that
/// silently did nothing would leave a stale table looking fresh.
fn which_go() -> Result<(), String> {
    let found = Command::new("go")
        .arg("version")
        .output()
        .map_err(|e| format!("`go version` could not be run: {e}"))?;
    if !found.status.success() {
        return Err("`go version` failed; this command needs a Go toolchain on PATH".into());
    }
    println!("==> {}", String::from_utf8_lossy(&found.stdout).trim());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generator directories.
    fn generator_dirs(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        GENERATORS
            .iter()
            .map(|(name, _)| root.join("ci/go-golden").join(name))
            .collect()
    }

    /// Every generator exists, carries a `go.mod`, and names no absolute path.
    ///
    /// The absolute path is the specific thing M4-3's code review found: one generator
    /// hard-coded `/work/<...>/workdir/...`, so it could not run anywhere else and the
    /// golden it produced could not be re-derived by anyone.
    #[test]
    fn every_generator_is_committed_and_relocatable() {
        let root = crate::repo_root();
        for dir in generator_dirs(&root) {
            assert!(dir.is_dir(), "{} does not exist", dir.display());
            assert!(
                dir.join("go.mod").is_file(),
                "{} has no go.mod, so `go run .` cannot work",
                dir.display()
            );
            let source = std::fs::read_to_string(dir.join("main.go"))
                .unwrap_or_else(|e| panic!("{}/main.go: {e}", dir.display()));
            assert!(
                !source.contains("= \"/"),
                "{}/main.go names an absolute path, so it only runs where it was written",
                dir.display()
            );
        }
    }

    /// And every table the command writes is in the tree, so a reader who finds the
    /// generator finds its output.
    #[test]
    fn every_golden_the_command_writes_exists() {
        let root = crate::repo_root();
        for (name, out) in GENERATORS {
            let path = root.join(out);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert!(
                text.contains("cargo xtask go-golden"),
                "{out} does not say how to regenerate it; run `cargo xtask go-golden`"
            );
            assert!(
                text.contains(&format!("ci/go-golden/{name}")),
                "{out} does not name the generator that produced it"
            );
            assert!(
                text.lines().filter(|l| !l.starts_with('#')).count() > 50,
                "{out} is suspiciously short"
            );
        }
    }

    #[test]
    fn the_command_takes_no_flags() {
        let err = main(&["--check".to_string()]).expect_err("no flags");
        assert!(err.contains("takes no flags"), "{err}");
    }
}
