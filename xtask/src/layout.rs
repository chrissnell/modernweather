//! `cargo xtask gate-layout` — the crate-boundary gate declared in `ci/layers.toml` and
//! armed by `ci/gates.toml` as the gate `layout`.
//!
//! Why it exists. W3 decision 1's argument for eighteen crates rather than one is that
//! "forbidden to depend on" is only enforceable as a `Cargo.toml` fact: any module in one
//! crate can write `use crate::store;`, whereas a crate that does not list `mw-store` in
//! `[dependencies]` cannot, and `cargo build` says so. That covers the edges that exist.
//! It does not cover the edges that must never be ADDED — which is one reviewable line —
//! nor any of the rules that are about source text rather than about a manifest: "only
//! `mw-types` mentions `generated/`", "only `mw-http` binds a socket", "only the binary
//! depends on `anyhow`", the one zero-fill, and W3 decision 81's template escaping rule.
//! This is the only enforcement those have. docs/reconciliation.md G-4 is what happens
//! without it.
//!
//! Everything the gate knows is declared in `ci/layers.toml`; nothing is hard-coded here
//! but the shape of the checks.
//!
//! Exit codes: 0 clean, 1 violations found, 2 the declaration or the tree is unreadable.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Deserialize;

const LAYERS: &str = "ci/layers.toml";
const CRATES: &str = "crates";

// --------------------------------------------------------------------------- declaration

#[derive(Debug, Deserialize)]
pub struct Layers {
    /// Lowest layer first. A crate may depend only on a strictly lower layer.
    pub layers: Vec<String>,
    #[serde(rename = "crate")]
    pub crates: Vec<CrateDecl>,
    #[serde(default, rename = "dep_rule")]
    pub dep_rules: Vec<DepRule>,
    #[serde(default, rename = "source_rule")]
    pub source_rules: Vec<SourceRule>,
    pub templates: Templates,
    pub generated: GeneratedDecl,
}

#[derive(Debug, Deserialize)]
pub struct CrateDecl {
    /// The directory under `crates/` and the package name, which must agree.
    pub name: String,
    pub layer: String,
    #[serde(default)]
    pub responsibility: String,
    /// Crate names this one may not name in any dependency table.
    #[serde(default)]
    pub forbidden: Vec<String>,
    /// Strings this crate's own source may not contain. For the properties a manifest
    /// cannot express — a pure codec's `chrono` is a timestamp type until someone calls
    /// `now()`.
    #[serde(default)]
    pub forbidden_source: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct DepRule {
    pub id: String,
    pub dependency: String,
    pub only: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct SourceRule {
    pub id: String,
    pub needles: Vec<String>,
    pub only: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct Templates {
    pub dir: String,
    pub needles: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct GeneratedDecl {
    pub dir: String,
    pub header: String,
    pub reason: String,
}

pub fn load(root: &Path) -> Result<Layers, String> {
    let path = root.join(LAYERS);
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let decl: Layers =
        toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    decl.self_check()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(decl)
}

impl Layers {
    /// A declaration that contradicts itself would otherwise surface as a confusing
    /// violation rather than as a problem with the declaration, so it fails the load.
    fn self_check(&self) -> Result<(), String> {
        if self.layers.is_empty() {
            return Err("`layers` is empty, so no layer order is declared".into());
        }
        let mut seen = BTreeSet::new();
        for l in &self.layers {
            if !seen.insert(l) {
                return Err(format!("layer `{l}` is declared twice"));
            }
        }
        let mut names = BTreeSet::new();
        for c in &self.crates {
            if !names.insert(&c.name) {
                return Err(format!("crate `{}` is declared twice", c.name));
            }
            if self.rank(&c.layer).is_none() {
                return Err(format!(
                    "crate `{}` declares layer `{}`, which is not in `layers`",
                    c.name, c.layer
                ));
            }
        }
        for rule in &self.dep_rules {
            for name in &rule.only {
                if !names.contains(name) {
                    return Err(format!(
                        "dep_rule `{}` exempts `{name}`, which is not a declared crate",
                        rule.id
                    ));
                }
            }
        }
        for rule in &self.source_rules {
            if rule.needles.is_empty() {
                return Err(format!("source_rule `{}` declares no needles", rule.id));
            }
            for name in &rule.only {
                if !names.contains(name) {
                    return Err(format!(
                        "source_rule `{}` exempts `{name}`, which is not a declared crate",
                        rule.id
                    ));
                }
            }
        }
        Ok(())
    }

    fn rank(&self, layer: &str) -> Option<usize> {
        self.layers.iter().position(|l| l == layer)
    }

    fn decl(&self, name: &str) -> Option<&CrateDecl> {
        self.crates.iter().find(|c| c.name == name)
    }
}

// ------------------------------------------------------------------------------ manifests

#[derive(Debug, Deserialize)]
struct Manifest {
    package: Option<PackageSection>,
    #[serde(default)]
    dependencies: BTreeMap<String, Dep>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: BTreeMap<String, Dep>,
    #[serde(default, rename = "build-dependencies")]
    build_dependencies: BTreeMap<String, Dep>,
    workspace: Option<WorkspaceSection>,
}

#[derive(Debug, Deserialize)]
struct PackageSection {
    name: String,
}

#[derive(Debug, Deserialize)]
struct WorkspaceSection {
    #[serde(default)]
    dependencies: BTreeMap<String, Dep>,
}

/// `foo = "1"` or `foo = { ... }`. Only two fields matter: `package`, which renames, and
/// `workspace`, which defers the rename to `[workspace.dependencies]`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Dep {
    /// `foo = "1"`. A bare version string cannot rename, so its content is never read —
    /// the variant exists so that the untagged match does not fall through to `Table`.
    Version(#[allow(dead_code)] String),
    Table(DepTable),
}

#[derive(Debug, Default, Deserialize)]
struct DepTable {
    package: Option<String>,
    workspace: Option<bool>,
}

/// One dependency as the gate sees it: where it was declared, and what crate it actually
/// resolves to. `log = { package = "tracing" }` is not a `log` dependency, and
/// `shout = { workspace = true }` over a root entry of `shout = { package = "anyhow" }`
/// is one — resolving both is what stops the forbidden lists from being a spelling test.
#[derive(Debug)]
struct Declared {
    /// The effective crate name.
    name: String,
    /// `dependencies`, `dev-dependencies` or `build-dependencies`.
    table: &'static str,
    /// The key as written, when it differs from the effective name.
    alias: Option<String>,
}

impl Declared {
    fn as_written(&self) -> String {
        match &self.alias {
            Some(key) => format!("{} (declared as `{key}`)", self.name),
            None => self.name.clone(),
        }
    }
}

fn read_manifest(path: &Path) -> Result<Manifest, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))
}

/// `[workspace.dependencies]` key -> effective crate name, so that `foo.workspace = true`
/// in a crate can be resolved through it.
fn workspace_renames(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let m = read_manifest(&root.join("Cargo.toml"))?;
    let mut out = BTreeMap::new();
    for (key, dep) in m.workspace.map(|w| w.dependencies).unwrap_or_default() {
        let effective = match &dep {
            Dep::Table(t) => t.package.clone().unwrap_or_else(|| key.clone()),
            Dep::Version(_) => key.clone(),
        };
        out.insert(key, effective);
    }
    Ok(out)
}

fn declared_deps(m: &Manifest, renames: &BTreeMap<String, String>) -> Vec<Declared> {
    let tables: [(&'static str, &BTreeMap<String, Dep>); 3] = [
        ("dependencies", &m.dependencies),
        ("dev-dependencies", &m.dev_dependencies),
        ("build-dependencies", &m.build_dependencies),
    ];
    let mut out = vec![];
    for (table, deps) in tables {
        for (key, dep) in deps {
            let name = match dep {
                Dep::Version(_) => key.clone(),
                Dep::Table(t) => match (&t.package, t.workspace) {
                    (Some(p), _) => p.clone(),
                    (None, Some(true)) => renames.get(key).cloned().unwrap_or_else(|| key.clone()),
                    (None, _) => key.clone(),
                },
            };
            let alias = if name == *key {
                None
            } else {
                Some(key.clone())
            };
            out.push(Declared { name, table, alias });
        }
    }
    out
}

// ----------------------------------------------------------------------------- violations

#[derive(Debug)]
pub struct Violation {
    pub rule: String,
    pub what: String,
    pub reason: String,
}

const LAYER_REASON: &str = "\
docs/service-architecture.md §1.4 declares one layer order over the eighteen crates, and
ci/layers.toml is that order. A dependency on the same layer or a higher one is how the
graph acquires a cycle and how a crate acquires reach it is not supposed to have; §1.5's
general statement is that accretion requires a type that can reach the thing being
accreted, and the layer order is what removes the reach.";

const FORBIDDEN_REASON: &str = "\
The `Forbidden to depend on` column of docs/service-architecture.md §1.2, which is the
column that matters: that table is only a description until something rejects the edge.";

const ROSTER_REASON: &str = "\
Every crate under crates/ must be declared in ci/layers.toml and every declaration must
exist on disk. Without both halves a new crate is outside the gate by simply not being
mentioned in it, which is docs/reconciliation.md G-1 in a new place: a check that reports
green because it never looked.";

// ---------------------------------------------------------------------------- the checks

/// Every check, over one tree. Takes the root as an argument rather than calling
/// `repo_root()` so that a test can build a scratch tree and watch the gate reject it —
/// M2-1's done-criteria require this gate to have been SEEN failing.
pub fn check(root: &Path) -> Result<Vec<Violation>, String> {
    let decl = load(root)?;
    let mut v = vec![];

    check_roster(root, &decl, &mut v)?;
    check_manifests(root, &decl, &mut v)?;
    check_sources(root, &decl, &mut v)?;
    check_templates(root, &decl, &mut v)?;
    check_generated(root, &decl, &mut v)?;

    Ok(v)
}

/// The declared set and the tree agree, in both directions.
fn check_roster(root: &Path, decl: &Layers, v: &mut Vec<Violation>) -> Result<(), String> {
    let dir = root.join(CRATES);
    let mut on_disk = BTreeSet::new();
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() && p.join("Cargo.toml").exists() {
            on_disk.insert(
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    let declared: BTreeSet<String> = decl.crates.iter().map(|c| c.name.clone()).collect();

    for name in on_disk.difference(&declared) {
        v.push(Violation {
            rule: "roster".into(),
            what: format!(
                "crates/{name}/ has a Cargo.toml but no [[crate]] in {LAYERS}, so no layer \
                 and no forbidden list apply to it"
            ),
            reason: ROSTER_REASON.into(),
        });
    }
    for name in declared.difference(&on_disk) {
        v.push(Violation {
            rule: "roster".into(),
            what: format!(
                "{LAYERS} declares `{name}`, but crates/{name}/Cargo.toml does not exist"
            ),
            reason: ROSTER_REASON.into(),
        });
    }
    Ok(())
}

/// The layer order, each crate's forbidden list, and the `[[dep_rule]]`s.
fn check_manifests(root: &Path, decl: &Layers, v: &mut Vec<Violation>) -> Result<(), String> {
    let renames = workspace_renames(root)?;
    let workspace_crates: BTreeSet<&str> = decl.crates.iter().map(|c| c.name.as_str()).collect();

    for c in &decl.crates {
        let manifest_path = root.join(CRATES).join(&c.name).join("Cargo.toml");
        if !manifest_path.exists() {
            continue; // already reported by check_roster
        }
        let m = read_manifest(&manifest_path)?;

        if let Some(pkg) = &m.package {
            if pkg.name != c.name {
                v.push(Violation {
                    rule: "roster".into(),
                    what: format!(
                        "crates/{}/Cargo.toml declares package `{}`; the directory name and the \
                         package name must agree, because {LAYERS} addresses crates by one name",
                        c.name, pkg.name
                    ),
                    reason: ROSTER_REASON.into(),
                });
            }
        }

        let here = decl.rank(&c.layer).expect("checked at load");
        for dep in declared_deps(&m, &renames) {
            // The layer order, over workspace crates only.
            if workspace_crates.contains(dep.name.as_str()) {
                let there = decl
                    .decl(&dep.name)
                    .and_then(|d| decl.rank(&d.layer))
                    .expect("checked at load");
                if there >= here {
                    let their_layer = &decl.decl(&dep.name).expect("checked").layer;
                    v.push(Violation {
                        rule: "layer-order".into(),
                        what: format!(
                            "{} ({}) depends on {} ({}) in [{}] — a crate may depend only on a \
                             strictly lower layer",
                            c.name,
                            c.layer,
                            dep.as_written(),
                            their_layer,
                            dep.table
                        ),
                        reason: LAYER_REASON.into(),
                    });
                }
            }
            // The crate's own forbidden list.
            if c.forbidden.contains(&dep.name) {
                v.push(Violation {
                    rule: "forbidden".into(),
                    what: format!(
                        "{} declares {} in [{}], which {LAYERS} forbids it. {} is: {}",
                        c.name,
                        dep.as_written(),
                        dep.table,
                        c.name,
                        c.responsibility
                    ),
                    reason: FORBIDDEN_REASON.into(),
                });
            }
            // The cross-crate dependency rules.
            for rule in &decl.dep_rules {
                if rule.dependency == dep.name && !rule.only.contains(&c.name) {
                    let permitted = if rule.only.is_empty() {
                        "no crate may".to_string()
                    } else {
                        format!("only {} may", rule.only.join(", "))
                    };
                    v.push(Violation {
                        rule: rule.id.clone(),
                        what: format!(
                            "{} declares {} in [{}]; {permitted} depend on `{}`",
                            c.name,
                            dep.as_written(),
                            dep.table,
                            rule.dependency
                        ),
                        reason: rule.reason.trim().to_string(),
                    });
                }
            }
        }
    }
    Ok(())
}

/// The `[[source_rule]]`s and each crate's `forbidden_source`.
fn check_sources(root: &Path, decl: &Layers, v: &mut Vec<Violation>) -> Result<(), String> {
    for c in &decl.crates {
        let dir = root.join(CRATES).join(&c.name);
        for (path, text) in rust_files(&dir) {
            let rel = crate::rel_path(&path, root);
            for rule in &decl.source_rules {
                if rule.only.contains(&c.name) {
                    continue;
                }
                for needle in &rule.needles {
                    for line in hits(&text, needle) {
                        v.push(Violation {
                            rule: rule.id.clone(),
                            what: format!("{rel}:{line} contains `{needle}` ({} may not)", c.name),
                            reason: rule.reason.trim().to_string(),
                        });
                    }
                }
            }
            for needle in &c.forbidden_source {
                for line in hits(&text, needle) {
                    v.push(Violation {
                        rule: "forbidden-source".into(),
                        what: format!(
                            "{rel}:{line} contains `{needle}`, which {LAYERS} forbids in {}. {} \
                             is: {}",
                            c.name, c.name, c.responsibility
                        ),
                        reason: FORBIDDEN_REASON.into(),
                    });
                }
            }
        }
    }
    Ok(())
}

/// W3 decision 81. Whitespace is stripped before matching, so `{{ x | safe }}` is caught
/// as well as `{{ x|safe }}`.
fn check_templates(root: &Path, decl: &Layers, v: &mut Vec<Violation>) -> Result<(), String> {
    let dir = root.join(&decl.templates.dir);
    if !dir.exists() {
        return Ok(()); // the website's templates arrive with the website
    }
    for (path, text) in files_under(&dir, None) {
        let rel = crate::rel_path(&path, root);
        for needle in &decl.templates.needles {
            let packed: String = needle.chars().filter(|c| !c.is_whitespace()).collect();
            for (i, line) in text.lines().enumerate() {
                let line: String = line.chars().filter(|c| !c.is_whitespace()).collect();
                if line.contains(&packed) {
                    v.push(Violation {
                        rule: "template-escaping".into(),
                        what: format!("{rel}:{} opts out of escaping with `{needle}`", i + 1),
                        reason: decl.templates.reason.trim().to_string(),
                    });
                }
            }
        }
    }
    Ok(())
}

/// The converse of the drift gate: nothing hand-written is ever placed in `generated/`.
///
/// The header is matched case-insensitively anywhere in the file rather than in its first
/// lines, because `generated/openapi/reading.json` is JSON and cannot carry a comment —
/// its notice lives in `info.description`, which is some way down a sorted object.
fn check_generated(root: &Path, decl: &Layers, v: &mut Vec<Violation>) -> Result<(), String> {
    let dir = root.join(&decl.generated.dir);
    if !dir.exists() {
        return Ok(());
    }
    let needle = decl.generated.header.to_lowercase();
    for (path, text) in files_under(&dir, None) {
        if !text.to_lowercase().contains(&needle) {
            v.push(Violation {
                rule: "generated-header".into(),
                what: format!(
                    "{} carries no `{}` notice, so it is either hand-written or its generator \
                     has stopped announcing itself",
                    crate::rel_path(&path, root),
                    decl.generated.header
                ),
                reason: decl.generated.reason.trim().to_string(),
            });
        }
    }
    Ok(())
}

/// 1-based line numbers of every line containing `needle`.
fn hits(text: &str, needle: &str) -> Vec<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| l.contains(needle))
        .map(|(i, _)| i + 1)
        .collect()
}

fn rust_files(dir: &Path) -> Vec<(PathBuf, String)> {
    files_under(dir, Some("rs"))
}

/// Every readable file under `dir`, optionally filtered by extension. `target/` is skipped
/// so that a crate-local build directory cannot fail the gate.
fn files_under(dir: &Path, ext: Option<&str>) -> Vec<(PathBuf, String)> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(p);
            } else {
                let keep = match ext {
                    Some(want) => p.extension().is_some_and(|e| e == want),
                    None => true,
                };
                if keep {
                    if let Ok(text) = std::fs::read_to_string(&p) {
                        out.push((p, text));
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ------------------------------------------------------------------------------ the entry

pub fn main(_args: &[String]) -> Result<ExitCode, String> {
    let root = crate::repo_root();
    let violations = check(&root)?;
    if violations.is_empty() {
        let decl = load(&root)?;
        println!(
            "layout: {} crates across {} layers; {} dependency rules and {} source rules hold",
            decl.crates.len(),
            decl.layers.len(),
            decl.dep_rules.len(),
            decl.source_rules.len()
        );
        return Ok(ExitCode::SUCCESS);
    }

    eprintln!("layout: {} violation(s) of {LAYERS}\n", violations.len());
    // Grouped by rule, with the declared reason printed once: the point of the gate is
    // that the person who hits it learns why the boundary is there, rather than being
    // told to delete a line.
    let mut by_rule: BTreeMap<&str, Vec<&Violation>> = BTreeMap::new();
    for x in &violations {
        by_rule.entry(&x.rule).or_default().push(x);
    }
    for (rule, group) in by_rule {
        eprintln!("  {rule}");
        for x in &group {
            eprintln!("    {}", x.what);
        }
        for line in group[0].reason.lines() {
            eprintln!("      {}", line.trim_end());
        }
        eprintln!();
    }
    Ok(ExitCode::from(1))
}

// ----------------------------------------------------------------------------------- test

/// **A boundary gate nobody has watched fail is docs/reconciliation.md G-1 again.** So
/// every rule below is exercised by building a scratch tree that breaks it and asserting
/// that the gate says so, not only by asserting that the real tree is clean.
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The shipped declaration, over the real tree. If this fails, the tree is wrong.
    #[test]
    fn the_workspace_satisfies_its_own_declaration() {
        let v = check(&crate::repo_root()).expect("ci/layers.toml loads");
        assert!(
            v.is_empty(),
            "{}",
            v.iter()
                .map(|x| format!("[{}] {}", x.rule, x.what))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn the_shipped_declaration_covers_every_crate_in_the_tree() {
        let root = crate::repo_root();
        let decl = load(&root).unwrap();
        assert_eq!(decl.crates.len(), 18, "W3 §1.2 decided eighteen crates");
        // The specific rules M2-1's done-criteria name, by id, so that deleting one from
        // ci/layers.toml fails a test rather than quietly shrinking the gate.
        for id in ["anyhow-only-at-the-binary-boundary", "one-logging-path"] {
            assert!(decl.dep_rules.iter().any(|r| r.id == id), "dep_rule {id}");
        }
        for id in [
            "only-mw-types-adopts-generated-rust",
            "only-mw-http-binds-a-socket",
            "one-zero-fill",
            "no-stdout-logging",
        ] {
            assert!(
                decl.source_rules.iter().any(|r| r.id == id),
                "source_rule {id}"
            );
        }
    }

    /// A scratch tree: a root manifest, `ci/layers.toml`, and one directory per crate.
    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        /// Two layers and three crates is enough to express every shape the real
        /// declaration does.
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let root = std::env::temp_dir().join(format!(
                "mw-layout-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&root);
            let me = Scratch { root };
            me.write(
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.dependencies]\n",
            );
            me.write(
                "ci/layers.toml",
                r#"
layers = ["low", "high"]

[[crate]]
name = "a-low"
layer = "low"
responsibility = "the bottom"
forbidden = ["sqlx"]
forbidden_source = ["::now("]

[[crate]]
name = "b-low"
layer = "low"
responsibility = "also the bottom"

[[crate]]
name = "c-high"
layer = "high"
responsibility = "the top"

[[dep_rule]]
id = "anyhow-at-the-boundary"
dependency = "anyhow"
only = ["c-high"]
reason = "only the binary may give up on classifying an error"

[[source_rule]]
id = "one-listener"
needles = ["TcpListener::bind"]
only = ["c-high"]
reason = "only one crate may bind a socket"

[templates]
dir = "crates/c-high/templates"
needles = ["|safe"]
reason = "escaping is not optional"

[generated]
dir = "generated"
header = "DO NOT EDIT"
reason = "nothing hand-written lives in generated/"
"#,
            );
            for name in ["a-low", "b-low", "c-high"] {
                me.crate_manifest(name, "");
                me.write(&format!("crates/{name}/src/lib.rs"), "");
            }
            me
        }

        fn write(&self, rel: &str, text: &str) {
            let p = self.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }

        fn crate_manifest(&self, name: &str, deps: &str) {
            self.write(
                &format!("crates/{name}/Cargo.toml"),
                &format!("[package]\nname = \"{name}\"\n\n[dependencies]\n{deps}"),
            );
        }

        fn check(&self) -> Vec<Violation> {
            super::check(&self.root).expect("the scratch declaration loads")
        }

        fn rules(&self) -> Vec<String> {
            self.check().into_iter().map(|v| v.rule).collect()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_clean_scratch_tree_passes() {
        let s = Scratch::new();
        assert!(s.check().is_empty(), "{:?}", s.check());
    }

    /// The headline case from M2-1's test strategy: add a forbidden dependency to a
    /// manifest and the gate rejects it.
    #[test]
    fn a_forbidden_dependency_is_rejected() {
        let s = Scratch::new();
        s.crate_manifest("a-low", "sqlx = \"0.8\"\n");
        let v = s.check();
        assert_eq!(s.rules(), vec!["forbidden"], "{v:?}");
        assert!(v[0].what.contains("a-low declares sqlx"), "{:?}", v[0].what);
    }

    /// A forbidden dependency hidden behind a rename is still that dependency.
    #[test]
    fn a_renamed_forbidden_dependency_is_rejected() {
        let s = Scratch::new();
        s.crate_manifest("a-low", "db = { package = \"sqlx\", version = \"0.8\" }\n");
        let v = s.check();
        assert_eq!(s.rules(), vec!["forbidden"], "{v:?}");
        assert!(v[0].what.contains("declared as `db`"), "{:?}", v[0].what);
    }

    /// ...including a rename that only `[workspace.dependencies]` knows about, which is
    /// the one a reviewer reading the crate's manifest would not see.
    #[test]
    fn a_rename_through_the_workspace_table_is_rejected() {
        let s = Scratch::new();
        s.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.dependencies]\n\
             shout = { package = \"anyhow\", version = \"1\" }\n",
        );
        s.crate_manifest("a-low", "shout.workspace = true\n");
        let v = s.check();
        assert_eq!(s.rules(), vec!["anyhow-at-the-boundary"], "{v:?}");
        assert!(v[0].what.contains("declared as `shout`"), "{:?}", v[0].what);
    }

    #[test]
    fn a_dev_dependency_counts_as_a_dependency() {
        let s = Scratch::new();
        s.write(
            "crates/a-low/Cargo.toml",
            "[package]\nname = \"a-low\"\n\n[dev-dependencies]\nsqlx = \"0.8\"\n",
        );
        let v = s.check();
        assert_eq!(s.rules(), vec!["forbidden"], "{v:?}");
        assert!(v[0].what.contains("[dev-dependencies]"), "{:?}", v[0].what);
    }

    #[test]
    fn a_build_dependency_counts_as_a_dependency() {
        let s = Scratch::new();
        s.write(
            "crates/a-low/Cargo.toml",
            "[package]\nname = \"a-low\"\n\n[build-dependencies]\nsqlx = \"0.8\"\n",
        );
        assert_eq!(s.rules(), vec!["forbidden"], "{:?}", s.check());
    }

    #[test]
    fn depending_upward_is_rejected() {
        let s = Scratch::new();
        s.crate_manifest("a-low", "c-high = { path = \"../c-high\" }\n");
        let v = s.check();
        assert_eq!(s.rules(), vec!["layer-order"], "{v:?}");
        assert!(
            v[0].what.contains("strictly lower layer"),
            "{:?}",
            v[0].what
        );
    }

    /// The rule is strictly lower, not lower-or-equal: §1.2 relies on it for the cases it
    /// states as a forbidden list between siblings — `mw-pipeline` must not reach
    /// `mw-store`, and both are in the service layer.
    #[test]
    fn depending_sideways_is_rejected() {
        let s = Scratch::new();
        s.crate_manifest("a-low", "b-low = { path = \"../b-low\" }\n");
        assert_eq!(s.rules(), vec!["layer-order"], "{:?}", s.check());
    }

    #[test]
    fn depending_downward_is_allowed() {
        let s = Scratch::new();
        s.crate_manifest("c-high", "a-low = { path = \"../a-low\" }\n");
        assert!(s.check().is_empty(), "{:?}", s.check());
    }

    #[test]
    fn a_dependency_rule_exempts_only_the_crates_it_names() {
        let s = Scratch::new();
        s.crate_manifest("c-high", "anyhow = \"1\"\n");
        assert!(s.check().is_empty(), "{:?}", s.check());
        s.crate_manifest("b-low", "anyhow = \"1\"\n");
        assert_eq!(s.rules(), vec!["anyhow-at-the-boundary"], "{:?}", s.check());
    }

    #[test]
    fn a_source_rule_rejects_the_needle_outside_the_crates_it_names() {
        let s = Scratch::new();
        s.write(
            "crates/b-low/src/serve.rs",
            "fn go() { let l = TcpListener::bind(\"0.0.0.0:80\"); }\n",
        );
        let v = s.check();
        assert_eq!(s.rules(), vec!["one-listener"], "{v:?}");
        assert!(v[0].what.contains("serve.rs:1"), "{:?}", v[0].what);
        // The same line in the crate the rule names is fine.
        let s2 = Scratch::new();
        s2.write(
            "crates/c-high/src/serve.rs",
            "fn go() { let l = TcpListener::bind(\"0.0.0.0:80\"); }\n",
        );
        assert!(s2.check().is_empty(), "{:?}", s2.check());
    }

    /// `tests/` and `build.rs` are scanned too: a rule that only looked at `src/` would
    /// miss the build script, which is exactly where a path into `generated/` would go.
    #[test]
    fn source_rules_cover_tests_and_build_scripts() {
        for rel in ["crates/b-low/tests/it.rs", "crates/b-low/build.rs"] {
            let s = Scratch::new();
            s.write(rel, "// TcpListener::bind\n");
            assert_eq!(s.rules(), vec!["one-listener"], "{rel}: {:?}", s.check());
        }
    }

    /// The property a manifest cannot express: a pure crate's `chrono` is a timestamp
    /// type until someone calls `now()`.
    #[test]
    fn a_crates_own_forbidden_source_list_is_enforced() {
        let s = Scratch::new();
        s.write("crates/a-low/src/decode.rs", "let t = Utc::now();\n");
        let v = s.check();
        assert_eq!(s.rules(), vec!["forbidden-source"], "{v:?}");
        // b-low declares no forbidden_source, so the same line is fine there.
        let s2 = Scratch::new();
        s2.write("crates/b-low/src/decode.rs", "let t = Utc::now();\n");
        assert!(s2.check().is_empty(), "{:?}", s2.check());
    }

    #[test]
    fn an_undeclared_crate_in_the_tree_is_rejected() {
        let s = Scratch::new();
        s.crate_manifest("d-undeclared", "");
        let v = s.check();
        assert_eq!(s.rules(), vec!["roster"], "{v:?}");
        assert!(v[0].what.contains("no [[crate]]"), "{:?}", v[0].what);
    }

    #[test]
    fn a_declared_crate_missing_from_the_tree_is_rejected() {
        let s = Scratch::new();
        std::fs::remove_dir_all(s.root.join("crates/b-low")).unwrap();
        let v = s.check();
        assert_eq!(s.rules(), vec!["roster"], "{v:?}");
        assert!(v[0].what.contains("does not exist"), "{:?}", v[0].what);
    }

    /// Addressing a crate by its directory name is only sound while the two agree.
    #[test]
    fn a_package_name_that_disagrees_with_its_directory_is_rejected() {
        let s = Scratch::new();
        s.write(
            "crates/b-low/Cargo.toml",
            "[package]\nname = \"something-else\"\n",
        );
        assert_eq!(s.rules(), vec!["roster"], "{:?}", s.check());
    }

    #[test]
    fn a_template_that_opts_out_of_escaping_is_rejected() {
        for body in ["{{ name|safe }}", "{{ name | safe }}"] {
            let s = Scratch::new();
            s.write("crates/c-high/templates/index.html", body);
            assert_eq!(
                s.rules(),
                vec!["template-escaping"],
                "{body}: {:?}",
                s.check()
            );
        }
    }

    #[test]
    fn a_generated_file_without_the_header_is_rejected() {
        let s = Scratch::new();
        s.write("generated/rust/thing.rs", "pub struct Hand {}\n");
        let v = s.check();
        assert_eq!(s.rules(), vec!["generated-header"], "{v:?}");
        // The notice need not be a comment, because one of the shipped generated files is
        // JSON and cannot carry one.
        let s2 = Scratch::new();
        s2.write(
            "generated/openapi/x.json",
            "{\"info\":{\"x\":\"Do not edit.\"}}",
        );
        assert!(s2.check().is_empty(), "{:?}", s2.check());
    }

    #[test]
    fn a_declaration_that_contradicts_itself_fails_to_load() {
        let cases = [
            ("layers = []\n[[crate]]\nname=\"x\"\nlayer=\"a\"\n[templates]\ndir=\"t\"\nneedles=[]\nreason=\"r\"\n[generated]\ndir=\"g\"\nheader=\"H\"\nreason=\"r\"\n", "is empty"),
            ("layers = [\"a\", \"a\"]\n[[crate]]\nname=\"x\"\nlayer=\"a\"\n[templates]\ndir=\"t\"\nneedles=[]\nreason=\"r\"\n[generated]\ndir=\"g\"\nheader=\"H\"\nreason=\"r\"\n", "declared twice"),
            ("layers = [\"a\"]\n[[crate]]\nname=\"x\"\nlayer=\"nope\"\n[templates]\ndir=\"t\"\nneedles=[]\nreason=\"r\"\n[generated]\ndir=\"g\"\nheader=\"H\"\nreason=\"r\"\n", "not in `layers`"),
            ("layers = [\"a\"]\n[[crate]]\nname=\"x\"\nlayer=\"a\"\n[[source_rule]]\nid=\"r\"\nneedles=[\"n\"]\nonly=[\"ghost\"]\nreason=\"r\"\n[templates]\ndir=\"t\"\nneedles=[]\nreason=\"r\"\n[generated]\ndir=\"g\"\nheader=\"H\"\nreason=\"r\"\n", "not a declared crate"),
            ("layers = [\"a\"]\n[[crate]]\nname=\"x\"\nlayer=\"a\"\n[[source_rule]]\nid=\"r\"\nneedles=[]\nonly=[]\nreason=\"r\"\n[templates]\ndir=\"t\"\nneedles=[]\nreason=\"r\"\n[generated]\ndir=\"g\"\nheader=\"H\"\nreason=\"r\"\n", "no needles"),
        ];
        for (text, want) in cases {
            let s = Scratch::new();
            s.write("ci/layers.toml", text);
            let err = super::check(&s.root).expect_err("the declaration is contradictory");
            assert!(err.contains(want), "wanted {want:?}, got {err:?}");
        }
    }
}
