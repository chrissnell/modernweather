//! The deferred-gate mechanism described at the top of `ci/gates.toml`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, ExitCode, ExitStatus};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    #[serde(default, rename = "gate")]
    pub gates: Vec<Gate>,
}

#[derive(Debug, Deserialize)]
pub struct Gate {
    pub id: String,
    pub description: String,
    pub owner: String,
    /// Path, relative to the repository root, whose existence arms this gate.
    pub arms_when: String,
    pub cmd: Vec<String>,
    #[serde(default)]
    pub note: String,
    /// What to say for one exit code, keyed by the code as written in `ci/gates.toml`.
    ///
    /// C-W2-20: W2 D-18 gave `codegen --check` two failure codes — 1 is drift, 2 is a
    /// violated invariant — precisely so that the two could be reported differently, and
    /// then the runner mapped every failure to one string, so they read identically in
    /// CI. This is where a gate says the two sentences. A declared message replaces
    /// `note` for that code; `note` stays the catch-all for a code nobody declared.
    #[serde(default)]
    pub on_exit: BTreeMap<String, String>,
}

impl Gate {
    pub fn armed(&self) -> bool {
        self.armed_in(&crate::repo_root())
    }

    fn armed_in(&self, root: &Path) -> bool {
        root.join(&self.arms_when).exists()
    }

    fn message_for(&self, code: i32) -> Option<&str> {
        self.on_exit.get(&code.to_string()).map(String::as_str)
    }
}

/// What one gate did. The exit code travels in the value rather than being decided by
/// the caller, because the whole of C-W2-20 is that the command's own code has to reach
/// the person reading CI.
#[derive(Debug)]
pub enum Outcome {
    /// The arming path does not exist yet; the string is the report to print.
    Deferred(String),
    /// The command ran and succeeded.
    Passed,
    /// The command ran and failed. `exit` is what xtask exits with.
    Failed { exit: u8, report: String },
}

pub fn load() -> Result<Manifest, String> {
    let path = crate::repo_root().join("ci/gates.toml");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let manifest: Manifest =
        toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    check(&manifest).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(manifest)
}

/// A bad `on_exit` key would otherwise surface as a missing message at the one moment
/// the message is wanted — a red gate — so it fails the load of every command that reads
/// the manifest instead.
fn check(manifest: &Manifest) -> Result<(), String> {
    for gate in &manifest.gates {
        for (key, message) in &gate.on_exit {
            match key.parse::<i32>() {
                Ok(0) => {
                    return Err(format!(
                        "gate `{}` declares an on_exit message for 0, which is success",
                        gate.id
                    ))
                }
                Ok(1..=255) => {}
                Ok(other) => {
                    return Err(format!(
                        "gate `{}`: on_exit {other} is not a process exit code (1-255)",
                        gate.id
                    ))
                }
                Err(_) => {
                    return Err(format!(
                        "gate `{}`: on_exit key `{key}` is not an exit code",
                        gate.id
                    ))
                }
            }
            if message.trim().is_empty() {
                return Err(format!(
                    "gate `{}`: the on_exit message for {key} is empty",
                    gate.id
                ));
            }
        }
    }
    Ok(())
}

/// Used only to produce a better error when a gate names a subcommand that does not exist.
pub fn owner_of_cmd(subcommand: &str) -> Option<(String, String)> {
    let m = load().ok()?;
    m.gates
        .into_iter()
        .find(|g| {
            g.cmd.first().map(String::as_str) == Some("cargo")
                && g.cmd.get(2).map(String::as_str) == Some(subcommand)
        })
        .map(|g| (g.id, g.owner))
}

pub fn list(args: &[String]) -> Result<(), String> {
    let m = load()?;
    let markdown = args.iter().any(|a| a == "--markdown");

    if markdown {
        println!("| Gate | Armed | Arms when | Owner |");
        println!("|---|---|---|---|");
        for g in &m.gates {
            println!(
                "| `{}` | {} | `{}` | {} |",
                g.id,
                if g.armed() { "yes" } else { "**deferred**" },
                g.arms_when,
                g.owner
            );
        }
        println!();
        println!(
            "A deferred gate is one whose subject does not exist in the tree yet. It arms \
             itself when the path above appears — see `ci/gates.toml`."
        );
        return Ok(());
    }

    let width = m.gates.iter().map(|g| g.id.len()).max().unwrap_or(4);
    for g in &m.gates {
        println!(
            "{:<width$}  {:<8}  arms when {} exists  ({})",
            g.id,
            if g.armed() { "ARMED" } else { "DEFERRED" },
            g.arms_when,
            g.owner,
            width = width
        );
        println!("{:<width$}  {}", "", g.description, width = width);
    }
    Ok(())
}

pub fn run(args: &[String]) -> Result<ExitCode, String> {
    let id = args
        .first()
        .ok_or_else(|| "usage: cargo xtask gate <id>".to_string())?;
    let m = load()?;
    let gate = m
        .gates
        .iter()
        .find(|g| &g.id == id)
        .ok_or_else(|| format!("no gate `{id}` in ci/gates.toml"))?;

    match evaluate(&crate::repo_root(), gate)? {
        Outcome::Deferred(report) => {
            println!("{report}");
            Ok(ExitCode::SUCCESS)
        }
        Outcome::Passed => Ok(ExitCode::SUCCESS),
        Outcome::Failed { exit, report } => {
            eprintln!("{report}");
            Ok(ExitCode::from(exit))
        }
    }
}

/// Runs one gate. `Err` is reserved for the runner's own problems — an empty `cmd`, a
/// command that could not be spawned — which are bad usage and exit 2; a command that
/// ran and failed is an `Outcome`, carrying its own code.
pub fn evaluate(root: &Path, gate: &Gate) -> Result<Outcome, String> {
    if !gate.armed_in(root) {
        return Ok(Outcome::Deferred(format!(
            "DEFERRED  {}\n  {}\n  owned by {}\n  \
             arms itself when {} exists; nothing to check until then",
            gate.id, gate.description, gate.owner, gate.arms_when
        )));
    }

    let (program, rest) = gate
        .cmd
        .split_first()
        .ok_or_else(|| format!("gate `{}` has an empty cmd", gate.id))?;
    println!("ARMED  {} -> {}", gate.id, gate.cmd.join(" "));

    let status = Command::new(resolve(root, program))
        .args(rest)
        .current_dir(root)
        .status()
        .map_err(|e| format!("running {}: {e}", gate.cmd.join(" ")))?;

    if status.success() {
        return Ok(Outcome::Passed);
    }
    let (exit, report) = failure(gate, status);
    Ok(Outcome::Failed { exit, report })
}

/// The command's exit code is carried out of xtask unchanged, so `cargo xtask gate
/// codegen-drift` exiting 1 still means drift and 2 still means the registry change was
/// rejected (W2 D-18).
fn failure(gate: &Gate, status: ExitStatus) -> (u8, String) {
    let mut lines = vec![];
    let exit = match status.code() {
        Some(code) => {
            lines.push(format!("gate `{}` failed (exit {code})", gate.id));
            match gate.message_for(code) {
                Some(message) => lines.extend(indented(message)),
                None => {
                    lines.extend(indented(&format!(
                        "ci/gates.toml declares no on_exit message for exit {code}, so the \
                         command's own output above is the diagnosis."
                    )));
                    lines.extend(indented(&gate.note));
                }
            }
            // Only a Windows process can report a code wider than a byte, and truncating
            // one would invent a verdict: 0x10000 would arrive as a success. One that does
            // not fit leaves as 2 instead, and the report above still names the real code.
            u8::try_from(code).unwrap_or(2)
        }
        // A signal leaves no code, so the command reached no verdict and must not be
        // given one of the command's own.
        None => {
            lines.push(format!(
                "gate `{}` was killed ({status}) without reaching an exit code, so it \
                 reported no verdict",
                gate.id
            ));
            lines.extend(indented(&gate.note));
            2
        }
    };
    (exit, lines.join("\n"))
}

fn indented(text: &str) -> Vec<String> {
    text.trim()
        .lines()
        .map(|l| format!("  {}", l.trim_end()))
        .collect()
}

/// A gate command may be a repository-relative script or a program on PATH.
fn resolve(root: &Path, program: &str) -> String {
    let candidate = root.join(program);
    if program.contains('/') && candidate.exists() {
        candidate.to_string_lossy().into_owned()
    } else {
        program.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One gate whose command is a stub exiting with `code`, declaring the same pair of
    /// messages `codegen-drift` declares in `ci/gates.toml`. `arms_when` names a file
    /// that is always in the tree, so the gate is armed and the command really runs.
    fn stub(code: i32) -> Manifest {
        let text = format!(
            r#"
            [[gate]]
            id = "stub"
            description = "a stub"
            owner = "this test"
            arms_when = "Cargo.toml"
            cmd = ["sh", "-c", "exit {code}"]
            note = "the catch-all note"
            on_exit.1 = "generated/ does not match the registry: regenerate and commit."
            on_exit.2 = "your registry change is not allowed."
            "#
        );
        let m: Manifest = toml::from_str(&text).unwrap();
        check(&m).unwrap();
        m
    }

    #[cfg(unix)]
    fn run_stub(code: i32) -> (u8, String) {
        let m = stub(code);
        match evaluate(&crate::repo_root(), &m.gates[0]).unwrap() {
            Outcome::Failed { exit, report } => (exit, report),
            other => panic!("a command that exits {code} must fail the gate; got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn exit_1_is_drift_and_leaves_xtask_exiting_1() {
        let (exit, report) = run_stub(1);
        assert_eq!(exit, 1, "{report}");
        assert!(report.contains("regenerate and commit"), "{report}");
        assert!(!report.contains("not allowed"), "{report}");
        // A declared message replaces the catch-all rather than being appended to it.
        assert!(!report.contains("catch-all"), "{report}");
    }

    #[cfg(unix)]
    #[test]
    fn exit_2_is_a_rejected_change_and_leaves_xtask_exiting_2() {
        let (exit, report) = run_stub(2);
        assert_eq!(exit, 2, "{report}");
        assert!(
            report.contains("your registry change is not allowed"),
            "{report}"
        );
        assert!(!report.contains("regenerate and commit"), "{report}");
    }

    #[cfg(unix)]
    #[test]
    fn an_undeclared_code_falls_back_to_the_note_and_is_still_carried_out() {
        // `cargo` itself exits 101 when the gate's command does not compile. Nobody
        // declares that, and inventing a verdict for it would be worse than saying so.
        let (exit, report) = run_stub(101);
        assert_eq!(exit, 101, "{report}");
        assert!(
            report.contains("no on_exit message for exit 101"),
            "{report}"
        );
        assert!(report.contains("the catch-all note"), "{report}");
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_succeeds_passes() {
        match evaluate(&crate::repo_root(), &stub(0).gates[0]).unwrap() {
            Outcome::Passed => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn a_deferred_gate_does_not_run_its_command() {
        let text = r#"
            [[gate]]
            id = "stub"
            description = "a stub"
            owner = "this test"
            arms_when = "no/such/path"
            cmd = ["false"]
        "#;
        let m: Manifest = toml::from_str(text).unwrap();
        match evaluate(&crate::repo_root(), &m.gates[0]).unwrap() {
            Outcome::Deferred(report) => assert!(report.starts_with("DEFERRED  stub"), "{report}"),
            other => panic!("got {other:?}"),
        }
    }

    /// The stub tests above would stay green if the messages were deleted from the
    /// shipped manifest, which is the only place `cargo xtask gate codegen-drift` reads
    /// them from.
    #[test]
    fn the_drift_gate_declares_both_of_its_codes() {
        let m = load().unwrap();
        let gate = m
            .gates
            .iter()
            .find(|g| g.id == "codegen-drift")
            .expect("ci/gates.toml declares codegen-drift");
        assert!(
            gate.message_for(1)
                .unwrap()
                .contains("regenerate and commit"),
            "exit 1 is drift: {:?}",
            gate.message_for(1)
        );
        assert!(
            gate.message_for(2)
                .unwrap()
                .contains("your registry change is not allowed"),
            "exit 2 is a violated invariant: {:?}",
            gate.message_for(2)
        );
    }

    #[test]
    fn a_message_for_success_is_rejected() {
        let m: Manifest = toml::from_str(
            r#"
            [[gate]]
            id = "stub"
            description = ""
            owner = ""
            arms_when = "Cargo.toml"
            cmd = ["true"]
            on_exit.0 = "green"
            "#,
        )
        .unwrap();
        assert!(check(&m).unwrap_err().contains("which is success"));
    }

    #[test]
    fn a_key_that_is_not_an_exit_code_is_rejected() {
        for (key, want) in [("drift", "is not an exit code"), ("256", "1-255")] {
            let m: Manifest = toml::from_str(&format!(
                r#"
                [[gate]]
                id = "stub"
                description = ""
                owner = ""
                arms_when = "Cargo.toml"
                cmd = ["true"]
                on_exit.{key} = "something"
                "#
            ))
            .unwrap();
            let err = check(&m).unwrap_err();
            assert!(err.contains(want), "key `{key}`: {err}");
        }
    }

    #[test]
    fn an_empty_message_is_rejected() {
        let m: Manifest = toml::from_str(
            r#"
            [[gate]]
            id = "stub"
            description = ""
            owner = ""
            arms_when = "Cargo.toml"
            cmd = ["true"]
            on_exit.1 = "   "
            "#,
        )
        .unwrap();
        assert!(check(&m).unwrap_err().contains("is empty"));
    }
}
