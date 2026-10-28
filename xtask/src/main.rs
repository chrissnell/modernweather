//! Repository tasks that the CI gate and the release pipeline both call.
//!
//! Everything here is also a developer command: a gate you cannot reproduce locally gets
//! worked around instead of fixed. `cargo xtask` lists what is available.

mod codegen;
mod coverage;
mod dist;
mod gates;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            usage();
            return ExitCode::FAILURE;
        }
    };

    let result = match cmd {
        "gates" => gates::list(rest),
        "gate" => gates::run(rest),
        "codegen" => codegen::main(rest),
        "coverage" => coverage::main(rest),
        "dist" => dist::main(rest),
        "targets" => dist::targets(rest),
        "help" | "-h" | "--help" => {
            usage();
            Ok(())
        }
        other => Err(unknown_subcommand(other)),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask {cmd}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A gate in `ci/gates.toml` can name a subcommand that the work item owning that gate
/// has not written yet. Failing loudly with the owner's name is the point: the
/// alternative is a gate that reports green because its command silently did nothing.
fn unknown_subcommand(name: &str) -> String {
    let owner = gates::owner_of_cmd(name);
    match owner {
        Some((id, owner)) => format!(
            "no such subcommand `{name}`.\n  \
             ci/gates.toml declares gate `{id}` with this command, owned by {owner}.\n  \
             That gate is armed, so the command has to exist. Implement it in xtask, or \
             correct the gate's `cmd`."
        ),
        None => format!("no such subcommand `{name}`. Run `cargo xtask help`."),
    }
}

fn usage() {
    eprintln!(
        "cargo xtask <command>

  gates [--markdown]            show every deferred gate and whether it is armed
  gate <id>                     run one gate, or report it as deferred and succeed
  codegen --check               regenerate from the metric registry and diff (never writes
                                into the working tree)
  coverage --check | --bump     compare line coverage against ci/coverage-floor.txt, or
                                raise the floor to the measured value
  dist --target <triple>        build and package one release artefact
                                [--version <v>] [--out <dir>]
  targets [--linux-only]        the release matrix from ci/release.toml, as JSON for a
                                GitHub Actions strategy.matrix
"
    );
}

/// Repository root, derived from this crate's manifest rather than the current directory
/// so that every command behaves the same wherever it is invoked from.
pub fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask/ always has a parent")
        .to_path_buf()
}

pub fn flag_value(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
    }
    None
}
