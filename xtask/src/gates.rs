//! The deferred-gate mechanism described at the top of `ci/gates.toml`.

use std::path::Path;
use std::process::Command;

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
}

impl Gate {
    pub fn armed(&self) -> bool {
        crate::repo_root().join(&self.arms_when).exists()
    }
}

pub fn load() -> Result<Manifest, String> {
    let path = crate::repo_root().join("ci/gates.toml");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))
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

pub fn run(args: &[String]) -> Result<(), String> {
    let id = args
        .first()
        .ok_or_else(|| "usage: cargo xtask gate <id>".to_string())?;
    let m = load()?;
    let gate = m
        .gates
        .iter()
        .find(|g| &g.id == id)
        .ok_or_else(|| format!("no gate `{id}` in ci/gates.toml"))?;

    if !gate.armed() {
        println!("DEFERRED  {}", gate.id);
        println!("  {}", gate.description);
        println!("  owned by {}", gate.owner);
        println!(
            "  arms itself when {} exists; nothing to check until then",
            gate.arms_when
        );
        return Ok(());
    }

    let (program, rest) = gate
        .cmd
        .split_first()
        .ok_or_else(|| format!("gate `{id}` has an empty cmd"))?;
    println!("ARMED  {} -> {}", gate.id, gate.cmd.join(" "));

    let program_path = resolve(program);
    let status = Command::new(&program_path)
        .args(rest)
        .current_dir(crate::repo_root())
        .status()
        .map_err(|e| format!("running {}: {e}", gate.cmd.join(" ")))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "gate `{id}` failed ({}). {}",
            status,
            gate.note.trim()
        ))
    }
}

/// A gate command may be a repository-relative script or a program on PATH.
fn resolve(program: &str) -> String {
    let candidate = crate::repo_root().join(program);
    if program.contains('/') && Path::new(&candidate).exists() {
        candidate.to_string_lossy().into_owned()
    } else {
        program.to_string()
    }
}
