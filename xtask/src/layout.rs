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
    /// Workspace members that are deliberately outside the gate. Recorded rather than
    /// implied, so a reader can tell exempt-by-design from forgotten.
    #[serde(default)]
    pub exempt_members: Vec<String>,
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
#[serde(deny_unknown_fields)]
pub struct SourceRule {
    pub id: String,
    pub needles: Vec<String>,
    pub only: Vec<String>,
    /// Repository-relative paths the rule exempts, for a rule whose legitimate call site
    /// is one FILE rather than one crate.
    ///
    /// `no-stdout-logging` is the case this exists for: §5.1 allows `println!` "only on
    /// the binary's argument-parsing path", and the crate was the finest unit this gate
    /// could express -- which made the rule a convention inside `crates/modernweather`
    /// and left a note saying the narrowing arrives with implementation-plan.md M2-4. It
    /// has. A path here is checked for existence by the self-check, so an exemption that
    /// outlives the file it names is a load failure rather than a silent widening.
    #[serde(default)]
    pub only_paths: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct Templates {
    pub dir: String,
    pub needles: Vec<String>,
    /// The `askama.toml` whose escaper table is pinned, relative to the repository root.
    ///
    /// **The hole the `needles` above cannot see.** An `askama.toml` that reassigns the
    /// `html` escaper turns escaping off globally without any filter appearing in any
    /// template -- `ci/layers.toml`'s own `reason` recorded that and said "the day one
    /// arrives it needs a rule of its own". M4-3 is the day: `crates/mw-web/askama.toml`
    /// registers `GoHtmlEscaper`, and this is the rule.
    pub escaper_config: String,
    /// The escaper path that file must name, and the only one it may.
    pub escaper_path: String,
    /// The extensions it must be registered for.
    pub escaper_extensions: Vec<String>,
    pub reason: String,
}

/// The shape of the `askama.toml` [`Templates::escaper_config`] names.
#[derive(Debug, Deserialize)]
struct AskamaConfig {
    #[serde(default)]
    escaper: Vec<AskamaEscaper>,
}

#[derive(Debug, Deserialize)]
struct AskamaEscaper {
    path: String,
    extensions: Vec<String>,
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
    // Path exemptions are checked against the tree, which `self_check` cannot see. An
    // exemption naming a file that no longer exists is how a narrowed rule quietly
    // becomes a wide one again.
    for rule in &decl.source_rules {
        for exempt in &rule.only_paths {
            if !root.join(exempt).is_file() {
                return Err(format!(
                    "{}: source_rule `{}` exempts `{exempt}`, which is not a file in the tree",
                    path.display(),
                    rule.id
                ));
            }
        }
    }
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
    /// `[target."cfg(unix)".dependencies]` and friends, keyed by the selector.
    ///
    /// These are real dependency edges on every target whose selector matches, and
    /// `cfg(unix)` matches all four release triples. A gate that read only the three
    /// unconditional tables would certify `[target."cfg(unix)".dependencies]` as clean
    /// while cargo linked it -- one reviewable line defeating the forbidden list, the layer
    /// order and the `anyhow` rule at once.
    #[serde(default)]
    target: BTreeMap<String, TargetSection>,
    workspace: Option<WorkspaceSection>,
}

#[derive(Debug, Default, Deserialize)]
struct TargetSection {
    #[serde(default)]
    dependencies: BTreeMap<String, Dep>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: BTreeMap<String, Dep>,
    #[serde(default, rename = "build-dependencies")]
    build_dependencies: BTreeMap<String, Dep>,
}

#[derive(Debug, Deserialize)]
struct PackageSection {
    name: String,
}

#[derive(Debug, Deserialize)]
struct WorkspaceSection {
    #[serde(default)]
    dependencies: BTreeMap<String, Dep>,
    #[serde(default)]
    members: Vec<String>,
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
    path: Option<String>,
}

/// One dependency as the gate sees it: where it was declared, and what crate it actually
/// resolves to. `log = { package = "tracing" }` is not a `log` dependency, and
/// `shout = { workspace = true }` over a root entry of `shout = { package = "anyhow" }`
/// is one — resolving both is what stops the forbidden lists from being a spelling test.
#[derive(Debug)]
struct Declared {
    /// The effective crate name.
    name: String,
    /// Which dependency table it was declared in, spelled as it appears in the manifest.
    table: String,
    /// The key as written, when it differs from the effective name.
    alias: Option<String>,
    /// How the version was decided, which is what `workspace-pinned-dependencies` checks.
    pinning: Pinning,
}

/// Where a dependency's version comes from.
#[derive(Debug, PartialEq, Eq)]
enum Pinning {
    /// `foo.workspace = true` — the version lives in `[workspace.dependencies]`.
    Workspace,
    /// `foo = { path = "..." }` — another crate in this repository.
    Path,
    /// `foo = "1"` or `foo = { version = "1" }` — a version decided in this manifest, which
    /// is the thing §1.1's "discipline of `[workspace.dependencies]`" forbids.
    Local,
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

/// Every dependency table a manifest can carry, including the platform-conditional ones.
/// Enumerating them here rather than at each call site is what stops the next table cargo
/// grows from being a silent hole.
fn dependency_tables(m: &Manifest) -> Vec<(String, &BTreeMap<String, Dep>)> {
    let mut tables = vec![
        ("dependencies".to_string(), &m.dependencies),
        ("dev-dependencies".to_string(), &m.dev_dependencies),
        ("build-dependencies".to_string(), &m.build_dependencies),
    ];
    for (selector, section) in &m.target {
        for (name, deps) in [
            ("dependencies", &section.dependencies),
            ("dev-dependencies", &section.dev_dependencies),
            ("build-dependencies", &section.build_dependencies),
        ] {
            tables.push((format!("target.\"{selector}\".{name}"), deps));
        }
    }
    tables
}

fn declared_deps(m: &Manifest, renames: &BTreeMap<String, String>) -> Vec<Declared> {
    let mut out = vec![];
    for (table, deps) in dependency_tables(m) {
        for (key, dep) in deps {
            let (name, pinning) = match dep {
                Dep::Version(_) => (key.clone(), Pinning::Local),
                Dep::Table(t) => {
                    let name = match (&t.package, t.workspace) {
                        (Some(p), _) => p.clone(),
                        (None, Some(true)) => {
                            renames.get(key).cloned().unwrap_or_else(|| key.clone())
                        }
                        (None, _) => key.clone(),
                    };
                    // `path` wins over a co-declared `version`: the path is what cargo
                    // actually builds in a workspace.
                    let pinning = if t.workspace == Some(true) {
                        Pinning::Workspace
                    } else if t.path.is_some() {
                        Pinning::Path
                    } else {
                        Pinning::Local
                    };
                    (name, pinning)
                }
            };
            let alias = if name == *key {
                None
            } else {
                Some(key.clone())
            };
            out.push(Declared {
                name,
                table: table.clone(),
                alias,
                pinning,
            });
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

const MEMBERS_REASON: &str = "\
Every `[workspace] members` entry must be inside crates/ and declared in ci/layers.toml, or
named in `exempt_members`. The roster check below covers crates/*; it does not cover a member
somewhere else, and a crate outside crates/ is outside every rule in this file -- layer,
forbidden list, anyhow, sockets, all of it.

Cargo also makes any path dependency inside the workspace directory a member automatically,
without it appearing in `members` at all. That half is covered by
`workspace-pinned-dependencies`, which requires a path dependency to name a declared crate.
The two checks together are what make \"declared in ci/layers.toml\" the only way into this
workspace.";

const PINNING_REASON: &str = "\
docs/service-architecture.md §1.1 names \"the discipline of [workspace.dependencies]\" as half
the cost of the eighteen-crate decision, and the root manifest says a substitution is an
amendment rather than a manifest edit. This is the mechanism for both, and without it neither
sentence is enforced by anything -- reconciliation.md G-4 in the file whose header is an essay
about G-4.

It also turns the forbidden lists from a denylist into an allowlist, which matters more than
the pinning: a denylist only rejects the crates somebody thought to name. DESIGN.md D-32 fixes
the default crate for each idiom precisely so that six agents do not each choose one, and a
crate that pins its own version of something is how a second HTTP client or a second TLS stack
arrives -- which is the cost §1.8 refuses for contract/conformance and would not notice here.

Two ways to satisfy it: `foo.workspace = true`, or a `path` to a crate declared in this file.";

const UNSCANNED_REASON: &str = "\
A .rs file under crates/ that is in no declared crate directory is read by no source rule, so
every needle in this file is blind to it. The module it belongs to is reachable through
`#[path]`, which is an established idiom in this tree -- crates/mw-types/src/generated/mod.rs
is exactly that -- so this is not a hypothetical shape. See also the `one-seam` source rule.";

const UNREADABLE_REASON: &str = "\
The gate could not read this file, so it checked nothing in it. A skip is not a pass: silently
ignoring the input is the shape of reconciliation.md G-1, and a generated file that is
unreadable would otherwise satisfy the do-not-edit header check by being unreadable.";

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

    check_members(root, &decl, &mut v)?;
    check_roster(root, &decl, &mut v)?;
    check_manifests(root, &decl, &mut v)?;
    check_sources(root, &decl, &mut v)?;
    check_templates(root, &decl, &mut v)?;
    check_generated(root, &decl, &mut v)?;

    Ok(v)
}

/// Every workspace member is inside crates/ and declared here, or explicitly exempt.
fn check_members(root: &Path, decl: &Layers, v: &mut Vec<Violation>) -> Result<(), String> {
    let m = read_manifest(&root.join("Cargo.toml"))?;
    let members = m.workspace.map(|w| w.members).unwrap_or_default();
    let declared: BTreeSet<&str> = decl.crates.iter().map(|c| c.name.as_str()).collect();
    let glob = format!("{CRATES}/*");

    for entry in &members {
        let covered = *entry == glob
            || decl.exempt_members.contains(entry)
            || entry
                .strip_prefix(&format!("{CRATES}/"))
                .is_some_and(|name| declared.contains(name));
        if !covered {
            v.push(Violation {
                rule: "members".into(),
                what: format!(
                    "the root manifest makes `{entry}` a workspace member, but it is neither \
                     `{glob}`, nor a crate declared in {LAYERS}, nor listed in \
                     `exempt_members`"
                ),
                reason: MEMBERS_REASON.into(),
            });
        }
    }
    // A stale exemption is worse than none: it reads as a deliberate decision about a crate
    // that is no longer there.
    for entry in &decl.exempt_members {
        if !members.contains(entry) {
            v.push(Violation {
                rule: "members".into(),
                what: format!(
                    "{LAYERS} exempts `{entry}`, which is not a workspace member -- the \
                     exemption is stale"
                ),
                reason: MEMBERS_REASON.into(),
            });
        }
    }
    Ok(())
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
            // The allowlist. Everything below this point is a denylist and only rejects
            // what somebody thought to name; this rejects anything that is not centrally
            // pinned or another declared crate, which is what makes the lists above a
            // complete answer rather than a partial one.
            match dep.pinning {
                Pinning::Workspace => {}
                Pinning::Path if workspace_crates.contains(dep.name.as_str()) => {}
                Pinning::Path => v.push(Violation {
                    rule: "workspace-pinned-dependencies".into(),
                    what: format!(
                        "{} declares {} in [{}] as a path dependency, and {} is not a crate \
                         declared in {LAYERS}",
                        c.name,
                        dep.as_written(),
                        dep.table,
                        dep.name
                    ),
                    reason: PINNING_REASON.into(),
                }),
                Pinning::Local => v.push(Violation {
                    rule: "workspace-pinned-dependencies".into(),
                    what: format!(
                        "{} pins its own version of {} in [{}]; it must be \
                         `{}.workspace = true` over an entry in [workspace.dependencies]",
                        c.name,
                        dep.as_written(),
                        dep.table,
                        dep.alias.as_deref().unwrap_or(&dep.name)
                    ),
                    reason: PINNING_REASON.into(),
                }),
            }
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
    check_every_source_is_scanned(root, decl, v);
    for c in &decl.crates {
        let dir = root.join(CRATES).join(&c.name);
        let (files, unreadable) = rust_files(&dir);
        for path in unreadable {
            v.push(unreadable_violation(&path, root));
        }
        for (path, text) in files {
            let rel = crate::rel_path(&path, root);
            for rule in &decl.source_rules {
                if rule.only.contains(&c.name) || rule.only_paths.contains(&rel) {
                    continue;
                }
                for (line, needles) in matches(&text, &rule.needles) {
                    v.push(Violation {
                        rule: rule.id.clone(),
                        what: format!(
                            "{rel}:{line} contains {} ({} may not)",
                            quoted(&needles),
                            c.name
                        ),
                        reason: rule.reason.trim().to_string(),
                    });
                }
            }
            for (line, needles) in matches(&text, &c.forbidden_source) {
                v.push(Violation {
                    rule: "forbidden-source".into(),
                    what: format!(
                        "{rel}:{line} contains {}, which {LAYERS} forbids in {}. {} is: {}",
                        quoted(&needles),
                        c.name,
                        c.name,
                        c.responsibility
                    ),
                    reason: FORBIDDEN_REASON.into(),
                });
            }
        }
    }
    Ok(())
}

/// Every `.rs` file under crates/ belongs to a declared crate, so every one of them is
/// reached by the loop above. `#[path]` makes "which directory is it in" and "which crate is
/// it part of" different questions, and only the first is enforceable here.
fn check_every_source_is_scanned(root: &Path, decl: &Layers, v: &mut Vec<Violation>) {
    let declared: BTreeSet<&str> = decl.crates.iter().map(|c| c.name.as_str()).collect();
    let crates_dir = root.join(CRATES);
    let (files, unreadable) = rust_files(&crates_dir);
    let paths = files.into_iter().map(|(p, _)| p).chain(unreadable);
    for path in paths {
        let Ok(rel) = path.strip_prefix(&crates_dir) else {
            continue;
        };
        // The first component is the crate directory; a file directly under crates/ has
        // only one component and belongs to nothing.
        let owner = (rel.components().count() > 1)
            .then(|| rel.components().next())
            .flatten()
            .map(|c| c.as_os_str().to_string_lossy().into_owned());
        if owner.as_deref().is_some_and(|n| declared.contains(n)) {
            continue;
        }
        v.push(Violation {
            rule: "unscanned-source".into(),
            what: format!(
                "{} is under {CRATES}/ but not inside a crate declared in {LAYERS}, so no \
                 source rule reads it",
                crate::rel_path(&path, root)
            ),
            reason: UNSCANNED_REASON.into(),
        });
    }
}

/// W3 decision 81. Whitespace is stripped before matching, so `{{ x | safe }}` is caught
/// as well as `{{ x|safe }}`.
fn check_templates(root: &Path, decl: &Layers, v: &mut Vec<Violation>) -> Result<(), String> {
    let dir = root.join(&decl.templates.dir);
    if !dir.exists() {
        return Ok(()); // the website's templates arrive with the website
    }
    let (files, unreadable) = files_under(&dir, None);
    for path in unreadable {
        v.push(unreadable_violation(&path, root));
    }
    for (path, text) in files {
        let rel = crate::rel_path(&path, root);
        for needle in &decl.templates.needles {
            for line in hits(&text, needle) {
                v.push(Violation {
                    rule: "template-escaping".into(),
                    what: format!("{rel}:{line} opts out of escaping with `{needle}`"),
                    reason: decl.templates.reason.trim().to_string(),
                });
            }
        }
    }
    check_askama_escaper(root, decl, v);
    Ok(())
}

/// The escaper `askama.toml` registers is the declared one, and the only one.
///
/// **This is the hole the template scan cannot see.** A `[[escaper]]` entry pointing at
/// `askama::filters::Empty` -- which writes its input unchanged -- turns escaping off for
/// every template in the crate, with no `safe` filter and no attribute anywhere for the
/// other two rules to find. Three ways to break it, all checked: a different path, a
/// missing extension, and a second entry (askama takes the **first** match, so an added
/// entry above the real one silently wins).
///
/// A missing or unparseable file is a violation rather than a skip: the rule exists because
/// the file exists, and "the gate stopped applying because the file was deleted" is the
/// failure mode every `only_paths` exemption in this module is checked against.
fn check_askama_escaper(root: &Path, decl: &Layers, v: &mut Vec<Violation>) {
    let rel = &decl.templates.escaper_config;
    let path = root.join(rel);
    let reason = decl.templates.reason.trim().to_string();
    let push = |v: &mut Vec<Violation>, what: String| {
        v.push(Violation {
            rule: "askama-escaper".into(),
            what,
            reason: reason.clone(),
        });
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            push(v, format!("{rel} cannot be read: {e}"));
            return;
        }
    };
    let config: AskamaConfig = match toml::from_str(&text) {
        Ok(config) => config,
        Err(e) => {
            push(v, format!("{rel} is not valid TOML: {e}"));
            return;
        }
    };
    if config.escaper.len() != 1 {
        push(
            v,
            format!(
                "{rel} declares {} escapers; exactly one is allowed, because askama takes \
                 the FIRST match and a second entry can silently win",
                config.escaper.len()
            ),
        );
        return;
    }
    let escaper = &config.escaper[0];
    if escaper.path != decl.templates.escaper_path {
        push(
            v,
            format!(
                "{rel} registers `{}` and must register `{}`",
                escaper.path, decl.templates.escaper_path
            ),
        );
    }
    if escaper.extensions != decl.templates.escaper_extensions {
        push(
            v,
            format!(
                "{rel} registers it for {:?} and must register it for {:?}",
                escaper.extensions, decl.templates.escaper_extensions
            ),
        );
    }
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
    let (files, unreadable) = files_under(&dir, None);
    for path in unreadable {
        v.push(unreadable_violation(&path, root));
    }
    for (path, text) in files {
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

/// 1-based line numbers of every line containing `needle`, ignoring whitespace on both
/// sides of the comparison — so `TcpListener :: bind` and `unwrap_or( 0.0 )` are the same
/// text as their idiomatic spellings, and a needle may be written readably.
///
/// This is a text gate and it is worth being precise about what that buys. It catches every
/// idiomatic spelling of the things it names, which is what makes a rule survive contact
/// with a codebase; it is not a sandbox, and an author who writes
/// `use tonic::transport::{Server as S}` to get around one has produced a line that a
/// reviewer sees. The structural half is the layer order and the forbidden lists: `mw-codec`
/// cannot write a listener because it cannot link `tokio` at all.
fn hits(text: &str, needle: &str) -> Vec<usize> {
    let packed = squeeze(needle);
    text.lines()
        .enumerate()
        .filter(|(_, l)| squeeze(l).contains(&packed))
        .map(|(i, _)| i + 1)
        .collect()
}

/// Line number -> every needle that matched it, so that one offending construct is one
/// violation however many spellings of it the rule lists. Overlapping needles are the
/// normal case — `unwrap_or(0.` and `unwrap_or(0.0)` describe the same mistake — and a
/// count inflated by the rule's own thoroughness is a count nobody can read.
fn matches(text: &str, needles: &[String]) -> BTreeMap<usize, Vec<String>> {
    let mut out: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for needle in needles {
        for line in hits(text, needle) {
            out.entry(line).or_default().push(needle.clone());
        }
    }
    out
}

fn quoted(needles: &[String]) -> String {
    needles
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(" and ")
}

fn squeeze(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn unreadable_violation(path: &Path, root: &Path) -> Violation {
    Violation {
        rule: "unreadable".into(),
        what: format!(
            "{} could not be read as UTF-8, so nothing in it was checked",
            crate::rel_path(path, root)
        ),
        reason: UNREADABLE_REASON.into(),
    }
}

type Scanned = (Vec<(PathBuf, String)>, Vec<PathBuf>);

fn rust_files(dir: &Path) -> Scanned {
    files_under(dir, Some("rs"))
}

/// Files under `dir`, optionally filtered by extension, as (readable, unreadable).
///
/// `target/` is skipped so that a crate-local build directory cannot fail the gate. A file
/// that is not valid UTF-8 comes back in the second list rather than being dropped: a check
/// that silently skips its input is the shape of G-1, and the callers turn it into a
/// violation.
fn files_under(dir: &Path, ext: Option<&str>) -> Scanned {
    let mut out = vec![];
    let mut unreadable = vec![];
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
                    match std::fs::read_to_string(&p) {
                        Ok(text) => out.push((p, text)),
                        Err(_) => unreadable.push(p),
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    unreadable.sort();
    (out, unreadable)
}

// ------------------------------------------------------------------------------ the entry

pub fn main(args: &[String]) -> Result<ExitCode, String> {
    // Every other xtask subcommand uses or rejects its flags. A typo'd flag that is a quiet
    // no-op on a gate is worse than usual: it reads as "I ran the gate with --fix".
    if let Some(unexpected) = args.first() {
        return Err(format!(
            "gate-layout takes no arguments, got `{unexpected}`. It reads {LAYERS} and the \
             tree, and has no modes."
        ));
    }
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
        // Every rule id the done-criteria or a design decision name, so that deleting one
        // from ci/layers.toml fails a test rather than quietly shrinking the gate.
        for id in ["anyhow-only-at-the-binary-boundary", "one-logging-path"] {
            assert!(decl.dep_rules.iter().any(|r| r.id == id), "dep_rule {id}");
        }
        for id in [
            "only-mw-types-adopts-generated-rust",
            "only-mw-http-binds-a-socket",
            "one-zero-fill",
            "no-stdout-logging",
            "one-seam",
            "no-template-escape-opt-out",
        ] {
            assert!(
                decl.source_rules.iter().any(|r| r.id == id),
                "source_rule {id}"
            );
        }
        // §5.1 allows `println!` on the binary's argument-parsing path and nowhere else.
        // The crate was the finest unit this gate could express until
        // implementation-plan.md M2-4 wrote that path; `only` being empty is what makes
        // the rule name files rather than a crate, and an exemption added back at crate
        // granularity would silence it for every handler in the binary.
        let stdout = decl
            .source_rules
            .iter()
            .find(|r| r.id == "no-stdout-logging")
            .expect("the rule exists");
        assert!(
            stdout.only.is_empty(),
            "no crate may be exempt from `no-stdout-logging`, only named files"
        );
        assert_eq!(
            stdout.only_paths,
            [
                "crates/modernweather/src/args.rs",
                "crates/mw-obs/src/stderr.rs",
                // A build script's only channel to cargo is stdout, and `println!` is the
                // only way to write a `cargo::` directive. Added by
                // implementation-plan.md M3-2 with `crates/mw-proto/build.rs`; see that
                // rule's reason in ci/layers.toml.
                "crates/mw-proto/build.rs"
            ],
            "the exempt set is the whole of this rule's narrowing, and widening it one \
             file at a time is the gradual version of the hole the mechanism closed"
        );
    }

    /// §1.2's load-bearing prohibitions, named here so that deleting one from
    /// ci/layers.toml fails a test. Only the pairs §1.2 states in prose are listed — the
    /// rest of each forbidden list is tightening, and the layer order covers the rest.
    #[test]
    fn the_shipped_declaration_keeps_every_prohibition_section_1_2_states() {
        let decl = load(&crate::repo_root()).unwrap();
        let required: &[(&str, &[&str])] = &[
            ("mw-types", &["tokio", "sqlx", "axum", "tonic", "serde"]),
            ("mw-proto", &["sqlx", "axum", "mw-store", "mw-api-legacy"]),
            ("mw-codec", &["tokio"]),
            ("mw-aprs", &["tokio", "sqlx"]),
            ("mw-obs", &["sqlx", "axum", "tonic"]),
            ("mw-component", &["sqlx"]),
            (
                "mw-supervisor",
                &["mw-store", "mw-pipeline", "mw-station", "mw-http"],
            ),
            ("mw-store", &["axum", "tonic", "mw-api-legacy", "mw-web"]),
            ("mw-pipeline", &["mw-store", "axum", "tonic"]),
            ("mw-station", &["mw-store", "mw-api-legacy", "mw-web"]),
            (
                "mw-http",
                &["mw-store", "mw-pipeline", "mw-api-legacy", "mw-web"],
            ),
            ("mw-api-legacy", &["sqlx", "mw-store"]),
            ("mw-web", &["mw-store", "mw-proto", "mw-api-legacy"]),
            ("mw-grpc", &["mw-store"]),
            ("mw-output", &["sqlx", "mw-store"]),
            ("mw-collector", &["mw-api-legacy", "mw-web"]),
            ("mw-api-mgmt", &["mw-store", "mw-pipeline"]),
        ];
        for (name, forbidden) in required {
            let c = decl
                .decl(name)
                .unwrap_or_else(|| panic!("{name} is declared"));
            for f in *forbidden {
                assert!(
                    c.forbidden.iter().any(|x| x == f),
                    "§1.2 forbids {name} -> {f}, and ci/layers.toml no longer says so"
                );
            }
        }
        // The binary is the one crate with no forbidden list, because it is the one place a
        // capability consumer and its provider may meet (§1.3 rule 1).
        assert!(decl.decl("modernweather").unwrap().forbidden.is_empty());
    }

    /// A scratch tree: a root manifest, `ci/layers.toml`, and one directory per crate.
    #[derive(Debug)]
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
            me.root_manifest("");
            // The escaper table `[templates]` pins. Written by default rather than per
            // test, because every tree a real check runs over has one -- and a missing file
            // is itself a violation.
            me.write(
                "askama.toml",
                "[[escaper]]\npath = \"crate::GoHtmlEscaper\"\nextensions = [\"html\"]\n",
            );
            // `r##` rather than `r#`: the declaration below contains `"#`, in the
            // `#[path` needle, which would close a single-hash raw string.
            me.write(
                "ci/layers.toml",
                r##"
layers = ["low", "high"]
exempt_members = []

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

[[dep_rule]]
id = "one-logging-path"
dependency = "log"
only = []
reason = "tracing only, everywhere"

[[source_rule]]
id = "one-listener"
needles = ["TcpListener::bind"]
only = ["c-high"]
reason = "only one crate may bind a socket"

[[source_rule]]
id = "one-printer"
needles = ["println!"]
only = []
only_paths = ["crates/c-high/src/print.rs"]
reason = "one file may reach a terminal"

[[source_rule]]
id = "one-seam"
needles = ["#[path"]
only = ["a-low"]
reason = "source lives where its crate owns it"

[templates]
dir = "crates/c-high/templates"
needles = ["|safe"]
escaper_config = "askama.toml"
escaper_path = "crate::GoHtmlEscaper"
escaper_extensions = ["html"]
reason = "escaping is not optional"

[generated]
dir = "generated"
header = "DO NOT EDIT"
reason = "nothing hand-written lives in generated/"
"##,
            );
            for name in ["a-low", "b-low", "c-high"] {
                me.crate_manifest(name, "");
                me.write(&format!("crates/{name}/src/lib.rs"), "");
            }
            // `one-printer` exempts this path, and a path exemption must name a file that
            // exists -- so the clean tree has to contain it. In `c-high` rather than
            // `b-low` because one case below removes that crate's whole directory, and a
            // missing exemption is a LOAD failure: it would turn that case's assertion
            // into this mechanism's error message.
            me.write("crates/c-high/src/print.rs", "");
            me
        }

        /// The root manifest. Third-party pins live here, which is what
        /// `workspace-pinned-dependencies` requires of every crate manifest, so the scratch
        /// tree has the same shape as the real one.
        fn root_manifest(&self, extra_members: &str) {
            self.write(
                "Cargo.toml",
                &format!(
                    "[workspace]\nmembers = [\"crates/*\"{extra_members}]\n\n\
                     [workspace.dependencies]\n\
                     sqlx = \"0.8\"\n\
                     anyhow = \"1\"\n\
                     log = \"0.4\"\n\
                     a-low = {{ path = \"crates/a-low\" }}\n\
                     b-low = {{ path = \"crates/b-low\" }}\n\
                     c-high = {{ path = \"crates/c-high\" }}\n"
                ),
            );
        }

        fn write(&self, rel: &str, text: &str) {
            let p = self.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }

        fn write_bytes(&self, rel: &str, bytes: &[u8]) {
            let p = self.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
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

        /// The distinct rule ids that fired, sorted. Set rather than list, because a rule
        /// firing on two lines is still one rule and the tests are about which boundary was
        /// crossed.
        fn rules(&self) -> Vec<String> {
            let mut out: Vec<String> = self.check().into_iter().map(|v| v.rule).collect();
            out.sort();
            out.dedup();
            out
        }

        fn what(&self, rule: &str) -> String {
            self.check()
                .into_iter()
                .find(|v| v.rule == rule)
                .map(|v| v.what)
                .unwrap_or_else(|| panic!("no `{rule}` violation in {:?}", self.rules()))
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

    // ---------------------------------------------------------------- manifest rules

    /// The headline case from M2-1's test strategy: add a forbidden dependency to a
    /// manifest and the gate rejects it.
    #[test]
    fn a_forbidden_dependency_is_rejected() {
        let s = Scratch::new();
        s.crate_manifest("a-low", "sqlx.workspace = true\n");
        assert_eq!(s.rules(), ["forbidden"], "{:?}", s.check());
        assert!(s.what("forbidden").contains("a-low declares sqlx"));
    }

    /// A forbidden dependency hidden behind a rename is still that dependency. It is also a
    /// locally pinned version, so both rules fire — which is the point of the allowlist.
    #[test]
    fn a_renamed_forbidden_dependency_is_rejected() {
        let s = Scratch::new();
        s.crate_manifest("a-low", "db = { package = \"sqlx\", version = \"0.8\" }\n");
        assert_eq!(
            s.rules(),
            ["forbidden", "workspace-pinned-dependencies"],
            "{:?}",
            s.check()
        );
        assert!(s.what("forbidden").contains("declared as `db`"));
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
        assert_eq!(s.rules(), ["anyhow-at-the-boundary"], "{:?}", s.check());
        assert!(s
            .what("anyhow-at-the-boundary")
            .contains("declared as `shout`"));
    }

    #[test]
    fn a_dev_dependency_counts_as_a_dependency() {
        let s = Scratch::new();
        s.write(
            "crates/a-low/Cargo.toml",
            "[package]\nname = \"a-low\"\n\n[dev-dependencies]\nsqlx.workspace = true\n",
        );
        assert_eq!(s.rules(), ["forbidden"], "{:?}", s.check());
        assert!(s.what("forbidden").contains("[dev-dependencies]"));
    }

    #[test]
    fn a_build_dependency_counts_as_a_dependency() {
        let s = Scratch::new();
        s.write(
            "crates/a-low/Cargo.toml",
            "[package]\nname = \"a-low\"\n\n[build-dependencies]\nsqlx.workspace = true\n",
        );
        assert_eq!(s.rules(), ["forbidden"], "{:?}", s.check());
        assert!(s.what("forbidden").contains("[build-dependencies]"));
    }

    /// `[target."cfg(unix)".dependencies]` is a real edge on all four release triples. A
    /// gate that read only the three unconditional tables certified it as clean — one line
    /// defeating the forbidden list, the layer order and the anyhow rule at once.
    #[test]
    fn a_platform_conditional_dependency_counts_as_a_dependency() {
        let s = Scratch::new();
        s.write(
            "crates/a-low/Cargo.toml",
            "[package]\nname = \"a-low\"\n\n\
             [target.\"cfg(unix)\".dependencies]\n\
             sqlx.workspace = true\n\
             anyhow.workspace = true\n\
             c-high.workspace = true\n",
        );
        assert_eq!(
            s.rules(),
            ["anyhow-at-the-boundary", "forbidden", "layer-order"],
            "{:?}",
            s.check()
        );
        assert!(
            s.what("forbidden")
                .contains("[target.\"cfg(unix)\".dependencies]"),
            "the message must name the table: {}",
            s.what("forbidden")
        );
    }

    #[test]
    fn a_platform_conditional_dev_or_build_dependency_counts_too() {
        for table in ["dev-dependencies", "build-dependencies"] {
            let s = Scratch::new();
            s.write(
                "crates/a-low/Cargo.toml",
                &format!(
                    "[package]\nname = \"a-low\"\n\n\
                     [target.\"cfg(target_os = \\\"linux\\\")\".{table}]\n\
                     sqlx.workspace = true\n"
                ),
            );
            assert_eq!(s.rules(), ["forbidden"], "{table}: {:?}", s.check());
        }
    }

    #[test]
    fn depending_upward_is_rejected() {
        let s = Scratch::new();
        s.crate_manifest("a-low", "c-high.workspace = true\n");
        assert_eq!(s.rules(), ["layer-order"], "{:?}", s.check());
        assert!(s.what("layer-order").contains("strictly lower layer"));
    }

    /// The rule is strictly lower, not lower-or-equal: §1.2 relies on it for the cases it
    /// states as a forbidden list between siblings — `mw-pipeline` must not reach
    /// `mw-store`, and both are in the service layer.
    #[test]
    fn depending_sideways_is_rejected() {
        let s = Scratch::new();
        s.crate_manifest("a-low", "b-low.workspace = true\n");
        assert_eq!(s.rules(), ["layer-order"], "{:?}", s.check());
    }

    #[test]
    fn depending_downward_is_allowed() {
        let s = Scratch::new();
        s.crate_manifest("c-high", "a-low.workspace = true\n");
        assert!(s.check().is_empty(), "{:?}", s.check());
    }

    #[test]
    fn a_dependency_rule_exempts_only_the_crates_it_names() {
        let s = Scratch::new();
        s.crate_manifest("c-high", "anyhow.workspace = true\n");
        assert!(s.check().is_empty(), "{:?}", s.check());
        s.crate_manifest("b-low", "anyhow.workspace = true\n");
        assert_eq!(s.rules(), ["anyhow-at-the-boundary"], "{:?}", s.check());
    }

    /// `only = []` means nobody, which is what the shipped `one-logging-path` rule says.
    /// Without this test that branch of the message is never produced.
    #[test]
    fn a_dependency_rule_with_an_empty_only_list_exempts_nobody() {
        for name in ["a-low", "b-low", "c-high"] {
            let s = Scratch::new();
            s.crate_manifest(name, "log.workspace = true\n");
            assert_eq!(s.rules(), ["one-logging-path"], "{name}: {:?}", s.check());
            assert!(
                s.what("one-logging-path").contains("no crate may"),
                "{}",
                s.what("one-logging-path")
            );
        }
    }

    // --------------------------------------------------- the workspace-pinning allowlist

    /// The allowlist. A denylist only rejects the crates somebody thought to name, so a
    /// crate that pins its own version of anything slips past every rule above.
    #[test]
    fn a_locally_pinned_dependency_is_rejected() {
        for spec in ["\"0.12\"", "{ version = \"0.12\" }"] {
            let s = Scratch::new();
            s.crate_manifest("a-low", &format!("reqwest = {spec}\n"));
            assert_eq!(
                s.rules(),
                ["workspace-pinned-dependencies"],
                "{spec}: {:?}",
                s.check()
            );
            assert!(s
                .what("workspace-pinned-dependencies")
                .contains("reqwest.workspace = true"));
        }
    }

    /// A path dependency is how a crate outside the declaration gets linked without ever
    /// appearing in `[workspace] members` — cargo makes it a member on its own.
    #[test]
    fn a_path_dependency_on_an_undeclared_crate_is_rejected() {
        let s = Scratch::new();
        s.crate_manifest("a-low", "sneaky = { path = \"../../lib/sneaky\" }\n");
        assert_eq!(
            s.rules(),
            ["workspace-pinned-dependencies"],
            "{:?}",
            s.check()
        );
        assert!(s
            .what("workspace-pinned-dependencies")
            .contains("is not a crate declared in"));
    }

    #[test]
    fn a_path_dependency_on_a_declared_crate_is_allowed() {
        let s = Scratch::new();
        s.crate_manifest("c-high", "a-low = { path = \"../a-low\" }\n");
        assert!(s.check().is_empty(), "{:?}", s.check());
    }

    // ------------------------------------------------------------------ workspace members

    #[test]
    fn a_workspace_member_outside_crates_is_rejected() {
        let s = Scratch::new();
        s.root_manifest(", \"lib/sneaky\"");
        assert_eq!(s.rules(), ["members"], "{:?}", s.check());
        assert!(s.what("members").contains("`lib/sneaky`"));
    }

    #[test]
    fn an_explicitly_exempt_member_is_allowed() {
        let s = Scratch::new();
        s.root_manifest(", \"tools/thing\"");
        let decl = std::fs::read_to_string(s.root.join("ci/layers.toml")).unwrap();
        s.write(
            "ci/layers.toml",
            &decl.replace("exempt_members = []", "exempt_members = [\"tools/thing\"]"),
        );
        assert!(s.check().is_empty(), "{:?}", s.check());
    }

    /// A stale exemption reads as a deliberate decision about a crate that is not there.
    #[test]
    fn a_stale_exemption_is_rejected() {
        let s = Scratch::new();
        let decl = std::fs::read_to_string(s.root.join("ci/layers.toml")).unwrap();
        s.write(
            "ci/layers.toml",
            &decl.replace("exempt_members = []", "exempt_members = [\"tools/gone\"]"),
        );
        assert_eq!(s.rules(), ["members"], "{:?}", s.check());
        assert!(s.what("members").contains("stale"));
    }

    /// An explicitly listed member under crates/ is fine as long as it is declared.
    #[test]
    fn an_explicitly_listed_declared_crate_is_allowed() {
        let s = Scratch::new();
        s.root_manifest(", \"crates/a-low\"");
        assert!(s.check().is_empty(), "{:?}", s.check());
    }

    // -------------------------------------------------------------------- the roster

    #[test]
    fn an_undeclared_crate_in_the_tree_is_rejected() {
        let s = Scratch::new();
        s.crate_manifest("d-undeclared", "");
        assert_eq!(s.rules(), ["roster"], "{:?}", s.check());
        assert!(s.what("roster").contains("no [[crate]]"));
    }

    #[test]
    fn a_declared_crate_missing_from_the_tree_is_rejected() {
        let s = Scratch::new();
        std::fs::remove_dir_all(s.root.join("crates/b-low")).unwrap();
        assert_eq!(s.rules(), ["roster"], "{:?}", s.check());
        assert!(s.what("roster").contains("does not exist"));
    }

    /// Addressing a crate by its directory name is only sound while the two agree.
    #[test]
    fn a_package_name_that_disagrees_with_its_directory_is_rejected() {
        let s = Scratch::new();
        s.write(
            "crates/b-low/Cargo.toml",
            "[package]\nname = \"something-else\"\n",
        );
        assert_eq!(s.rules(), ["roster"], "{:?}", s.check());
    }

    // -------------------------------------------------------------------- source rules

    #[test]
    fn a_source_rule_rejects_the_needle_outside_the_crates_it_names() {
        let s = Scratch::new();
        s.write(
            "crates/b-low/src/serve.rs",
            "fn go() { let l = TcpListener::bind(\"0.0.0.0:80\"); }\n",
        );
        assert_eq!(s.rules(), ["one-listener"], "{:?}", s.check());
        assert!(s.what("one-listener").contains("serve.rs:1"));
        // The same line in the crate the rule names is fine.
        let s2 = Scratch::new();
        s2.write(
            "crates/c-high/src/serve.rs",
            "fn go() { let l = TcpListener::bind(\"0.0.0.0:80\"); }\n",
        );
        assert!(s2.check().is_empty(), "{:?}", s2.check());
    }

    /// The narrowing implementation-plan.md M2-4 owed `no-stdout-logging`: an exemption
    /// that is one FILE rather than one crate.
    ///
    /// Proved able to fail in both directions, because an exemption nobody has watched
    /// reject anything is the same shape as a gate that never ran.
    #[test]
    fn a_path_exemption_covers_that_file_and_nothing_beside_it() {
        let s = Scratch::new();
        s.write(
            "crates/c-high/src/print.rs",
            "fn usage() { println!(\"usage\"); }\n",
        );
        assert!(
            s.check().is_empty(),
            "the exempt path may contain the needle: {:?}",
            s.check()
        );

        // One directory entry away, in the same crate, is a violation. This is the case
        // the crate-granular rule could not see.
        let s2 = Scratch::new();
        s2.write(
            "crates/c-high/src/other.rs",
            "fn debug() { println!(\"here\"); }\n",
        );
        assert_eq!(s2.rules(), ["one-printer"], "{:?}", s2.check());
        assert!(s2.what("one-printer").contains("other.rs:1"));
    }

    /// An exemption that outlives the file it names would quietly widen the rule back to
    /// the whole crate, so it fails the load rather than passing the check.
    #[test]
    fn a_path_exemption_naming_a_missing_file_fails_to_load() {
        let s = Scratch::new();
        std::fs::remove_file(s.root.join("crates/c-high/src/print.rs")).unwrap();
        let err = super::check(&s.root).expect_err("the exemption names nothing");
        assert!(err.contains("one-printer"), "{err}");
        assert!(err.contains("not a file in the tree"), "{err}");
    }

    /// Whitespace is not a hiding place. A needle may also be written readably.
    #[test]
    fn source_matching_ignores_whitespace() {
        let s = Scratch::new();
        s.write(
            "crates/b-low/src/serve.rs",
            "fn go() { let l = TcpListener :: bind(\"x\"); }\n",
        );
        assert_eq!(s.rules(), ["one-listener"], "{:?}", s.check());
    }

    /// `tests/` and `build.rs` are scanned too: a rule that only looked at `src/` would
    /// miss the build script, which is exactly where a path into `generated/` would go.
    #[test]
    fn source_rules_cover_tests_and_build_scripts() {
        for rel in ["crates/b-low/tests/it.rs", "crates/b-low/build.rs"] {
            let s = Scratch::new();
            s.write(rel, "// TcpListener::bind\n");
            assert_eq!(s.rules(), ["one-listener"], "{rel}: {:?}", s.check());
        }
    }

    /// The property a manifest cannot express: a pure crate's `chrono` is a timestamp
    /// type until someone calls `now()`.
    #[test]
    fn a_crates_own_forbidden_source_list_is_enforced() {
        let s = Scratch::new();
        s.write("crates/a-low/src/decode.rs", "let t = Utc::now();\n");
        assert_eq!(s.rules(), ["forbidden-source"], "{:?}", s.check());
        // b-low declares no forbidden_source, so the same line is fine there.
        let s2 = Scratch::new();
        s2.write("crates/b-low/src/decode.rs", "let t = Utc::now();\n");
        assert!(s2.check().is_empty(), "{:?}", s2.check());
    }

    /// One offending construct is one violation, however many of a rule's needles describe
    /// it. A count inflated by the rule's own thoroughness is a count nobody can read.
    #[test]
    fn overlapping_needles_report_one_violation_per_line() {
        let s = Scratch::new();
        let decl = std::fs::read_to_string(s.root.join("ci/layers.toml")).unwrap();
        s.write(
            "ci/layers.toml",
            &decl.replace(
                r#"needles = ["TcpListener::bind"]"#,
                r#"needles = ["TcpListener", "TcpListener::bind", "Listener::bind"]"#,
            ),
        );
        s.write("crates/b-low/src/serve.rs", "TcpListener::bind(\"x\");\n");
        let v = s.check();
        assert_eq!(v.len(), 1, "{v:?}");
        // ...and the message names every needle that matched, so nothing is hidden by the
        // deduplication.
        assert!(v[0].what.contains("`TcpListener`"), "{}", v[0].what);
        assert!(v[0].what.contains("`TcpListener::bind`"), "{}", v[0].what);
    }

    // --------------------------------------------- source the gate cannot see at all

    /// `#[path]` is the one way a module's source can live outside the directory its crate
    /// owns, which puts it where every source rule is blind. It is also the mechanism the
    /// real tree's own generated-code seam uses, so it reads as idiomatic rather than
    /// suspicious.
    #[test]
    fn a_path_attribute_outside_the_crate_that_owns_the_seam_is_rejected() {
        let s = Scratch::new();
        s.write(
            "crates/b-low/src/lib.rs",
            "#[path = \"../../../shared/x.rs\"]\npub mod x;\n",
        );
        assert_eq!(s.rules(), ["one-seam"], "{:?}", s.check());
        // a-low holds the seam in the scratch declaration, so it may.
        let s2 = Scratch::new();
        s2.write(
            "crates/a-low/src/lib.rs",
            "#[path = \"../../../shared/x.rs\"]\npub mod x;\n",
        );
        assert!(s2.check().is_empty(), "{:?}", s2.check());
    }

    /// A .rs file under crates/ that belongs to no declared crate is read by nothing.
    #[test]
    fn a_source_file_in_no_declared_crate_is_rejected() {
        for rel in ["crates/loose.rs", "crates/elsewhere/mod.rs"] {
            let s = Scratch::new();
            s.write(rel, "pub fn x() {}\n");
            assert_eq!(s.rules(), ["unscanned-source"], "{rel}: {:?}", s.check());
            assert!(s.what("unscanned-source").contains(rel), "{rel}");
        }
    }

    /// A file the gate cannot read is a violation, not a skip. A generated file that is not
    /// valid UTF-8 would otherwise satisfy the do-not-edit header check by being
    /// unreadable.
    #[test]
    fn a_file_the_gate_cannot_read_is_a_violation() {
        for rel in [
            "generated/x.bin",
            "crates/b-low/src/x.rs",
            "crates/c-high/templates/x.html",
        ] {
            let s = Scratch::new();
            s.write("generated/ok.rs", "// DO NOT EDIT\n");
            s.write_bytes(rel, &[0x66, 0x6f, 0xff, 0xfe, 0x6f]);
            assert!(
                s.rules().contains(&"unreadable".to_string()),
                "{rel}: {:?}",
                s.check()
            );
        }
    }

    // ---------------------------------------------------------------------- templates

    #[test]
    fn a_template_that_opts_out_of_escaping_is_rejected() {
        for body in ["{{ name|safe }}", "{{ name | safe }}"] {
            let s = Scratch::new();
            s.write("crates/c-high/templates/index.html", body);
            assert_eq!(s.rules(), ["template-escaping"], "{body}: {:?}", s.check());
        }
    }

    // ---------------------------------------------------------------------- generated

    #[test]
    fn a_generated_file_without_the_header_is_rejected() {
        let s = Scratch::new();
        s.write("generated/rust/thing.rs", "pub struct Hand {}\n");
        assert_eq!(s.rules(), ["generated-header"], "{:?}", s.check());
        // The notice need not be a comment, because one of the shipped generated files is
        // JSON and cannot carry one.
        let s2 = Scratch::new();
        s2.write(
            "generated/openapi/x.json",
            "{\"info\":{\"x\":\"Do not edit.\"}}",
        );
        assert!(s2.check().is_empty(), "{:?}", s2.check());
    }

    /// **The `askama.toml` hole, shown failing in all four of its shapes.**
    ///
    /// A gate that cannot fail is worth nothing, and this one is the only thing standing
    /// between `crates/mw-web/templates/` and globally disabled escaping: none of the
    /// needles above appears anywhere when the escaper itself is swapped.
    #[test]
    fn an_askama_config_that_disables_escaping_is_rejected() {
        let cases: [(&str, &str); 4] = [
            (
                "a different escaper",
                "[[escaper]]\npath = \"askama::filters::Empty\"\nextensions = [\"html\"]\n",
            ),
            (
                "a missing extension",
                "[[escaper]]\npath = \"crate::GoHtmlEscaper\"\nextensions = []\n",
            ),
            (
                "a second entry, which askama matches first",
                "[[escaper]]\npath = \"askama::filters::Empty\"\nextensions = [\"html\"]\n\
                 [[escaper]]\npath = \"crate::GoHtmlEscaper\"\nextensions = [\"html\"]\n",
            ),
            ("no escaper at all", "# nothing\n"),
        ];
        for (what, body) in cases {
            let s = Scratch::new();
            s.write("crates/c-high/templates/index.html", "{{ name }}\n");
            s.write("askama.toml", body);
            assert_eq!(s.rules(), ["askama-escaper"], "{what}: {:?}", s.check());
        }
        // And the file being gone is a violation rather than a skip.
        let s = Scratch::new();
        s.write("crates/c-high/templates/index.html", "{{ name }}\n");
        std::fs::remove_file(s.root.join("askama.toml")).unwrap();
        assert_eq!(s.rules(), ["askama-escaper"], "{:?}", s.check());
    }

    /// And the declared one passes, so the test above is not asserting that everything
    /// fails.
    #[test]
    fn the_declared_askama_escaper_passes() {
        let s = Scratch::new();
        s.write("crates/c-high/templates/index.html", "{{ name }}\n");
        assert!(s.check().is_empty(), "{:?}", s.check());
    }

    // ------------------------------------------------------------- the declaration itself

    #[test]
    fn a_declaration_that_contradicts_itself_fails_to_load() {
        let tail = "[templates]\ndir=\"t\"\nneedles=[]\n\
                    escaper_config=\"askama.toml\"\nescaper_path=\"p\"\n\
                    escaper_extensions=[]\nreason=\"r\"\n\
                    [generated]\ndir=\"g\"\nheader=\"H\"\nreason=\"r\"\n";
        let crate_decl = "[[crate]]\nname=\"x\"\nlayer=\"a\"\n";
        let cases = [
            (format!("layers = []\n{crate_decl}{tail}"), "is empty"),
            (
                format!("layers = [\"a\", \"a\"]\n{crate_decl}{tail}"),
                "declared twice",
            ),
            (
                format!("layers = [\"a\"]\n[[crate]]\nname=\"x\"\nlayer=\"nope\"\n{tail}"),
                "not in `layers`",
            ),
            (
                format!("layers = [\"a\"]\n{crate_decl}{crate_decl}{tail}"),
                "is declared twice",
            ),
            (
                format!(
                    "layers = [\"a\"]\n{crate_decl}\
                     [[source_rule]]\nid=\"r\"\nneedles=[\"n\"]\nonly=[\"ghost\"]\nreason=\"r\"\n{tail}"
                ),
                "not a declared crate",
            ),
            (
                format!(
                    "layers = [\"a\"]\n{crate_decl}\
                     [[source_rule]]\nid=\"r\"\nneedles=[]\nonly=[]\nreason=\"r\"\n{tail}"
                ),
                "no needles",
            ),
            (
                format!(
                    "layers = [\"a\"]\n{crate_decl}\
                     [[dep_rule]]\nid=\"r\"\ndependency=\"d\"\nonly=[\"ghost\"]\nreason=\"r\"\n{tail}"
                ),
                "not a declared crate",
            ),
        ];
        for (text, want) in cases {
            let s = Scratch::new();
            s.write("ci/layers.toml", &text);
            let err = super::check(&s.root).expect_err("the declaration is contradictory");
            assert!(err.contains(want), "wanted {want:?}, got {err:?}");
        }
    }

    /// Every other xtask subcommand uses or rejects its flags. A typo'd flag that is a
    /// quiet no-op on a gate reads as "I ran it with --fix".
    #[test]
    fn gate_layout_rejects_arguments() {
        let err = super::main(&["--fix".to_string()]).expect_err("no flags are accepted");
        assert!(err.contains("takes no arguments"), "{err}");
        assert!(err.contains("--fix"), "{err}");
    }
}
