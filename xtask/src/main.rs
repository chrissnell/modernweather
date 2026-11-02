//! `cargo xtask` — the ModernWeather repository task runner.
//!
//! Two things live here because two work items need the same entry point. W2 owns the
//! code generator: one registry (`metrics/registry.toml`) is the source of truth for
//! every metric, and everything else — table DDL, migrations, continuous-aggregate SQL,
//! Rust types, protobuf, OpenAPI, TypeScript — is a build artefact generated from it,
//! committed, and CI-checked for drift (D-20). W8 owns the gates and the release
//! pipeline, and every gate is also a developer command: a gate you cannot reproduce
//! locally gets worked around instead of fixed.
//!
//! `ci/gates.toml` declares the drift gate's command as `cargo xtask codegen --check`,
//! which is what W2 implements below, so W8's placeholder `codegen` module is gone.
//!
//! Exit codes:
//!   0  success, or `--check` found no drift
//!   1  `--check` found drift
//!   2  an invariant was violated, or usage was wrong
//!
//! `gate <id>` exits with its command's own code rather than a code of its own, so a
//! gate that runs `codegen --check` keeps the 1-means-drift, 2-means-rejected
//! distinction that code made (C-W2-20).

mod coverage;
mod dist;
mod gates;
mod gen;
mod legacy;
mod migrate;
mod registry;
mod snapshot;
mod validate;

use registry::Registry;
use snapshot::Snapshot;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};

const REGISTRY: &str = "metrics/registry.toml";
const SNAPSHOT: &str = "metrics/snapshot.toml";
const MIGRATIONS: &str = "migrations";
/// Wholly owned by the generator, which is what makes pruning it correct (N-15).
const GENERATED: &str = "generated";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = repo_root();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let owned: Vec<String> = args[1.min(args.len())..].to_vec();
    let flags: Vec<&str> = owned.iter().map(String::as_str).collect();

    // W8's commands report through Result<(), String>; W2's carry a meaningful exit code.
    let result = match cmd {
        "codegen" => codegen(&root, flags.contains(&"--check")),
        "migrate" => migrate_cmd(&root, &flags),
        "legacy-import" => legacy_import(&root, &flags),
        "budget" => budget(&root),
        "validate" => validate_cmd(&root),
        "gates" => gates::list(&owned).map(|()| ExitCode::SUCCESS),
        // Not mapped to a code of its own: the gate's command has already decided what
        // its failure means, and collapsing that was C-W2-20.
        "gate" => gates::run(&owned),
        "coverage" => coverage::main(&owned).map(|()| ExitCode::SUCCESS),
        "dist" => dist::main(&owned).map(|()| ExitCode::SUCCESS),
        "targets" => dist::targets(&owned).map(|()| ExitCode::SUCCESS),
        "help" | "--help" | "-h" => {
            usage();
            return ExitCode::SUCCESS;
        }
        other => {
            eprintln!("xtask: {}\n", unknown_subcommand(other));
            usage();
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("xtask {cmd}: {e}");
            ExitCode::from(2)
        }
    }
}

/// A gate in `ci/gates.toml` can name a subcommand that the work item owning that gate
/// has not written yet. Failing loudly with the owner's name is the point: the
/// alternative is a gate that reports green because its command silently did nothing.
fn unknown_subcommand(name: &str) -> String {
    match gates::owner_of_cmd(name) {
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
    println!(
        "cargo xtask <command>\n\
         \n\
         the metric registry (W2)\n\
         \x20 codegen            regenerate generated/ from metrics/registry.toml, removing\n\
         \x20                    any file there that the generator no longer writes\n\
         \x20 codegen --check    regenerate into a temporary directory and diff; never\n\
         \x20                    writes into the working tree. Exit 1 on drift.\n\
         \x20 migrate            diff the registry against metrics/snapshot.toml and emit a\n\
         \x20                    timestamped sqlx migration, then update the snapshot\n\
         \x20     --name <s>         migration name (default: derived from the changes)\n\
         \x20     --allow-breaking   permit column renames, type changes and strategy\n\
         \x20                        changes. Loud on purpose; read the migration.\n\
         \x20     --dry-run          print what would be written, write nothing\n\
         \x20 validate           check every invariant and exit\n\
         \n\
         the Go-to-Rust data migration (W9)\n\
         \x20 legacy-import      emit the migration SQL into ci/db/migrate-from-go/generated/\n\
         \x20                    from metrics/registry.toml and\n\
         \x20                    ci/db/migrate-from-go/go-aggregate-views.toml\n\
         \x20     --check            regenerate into a temporary directory and diff\n\
         \x20 budget             print the column budget against the ceiling\n\
         \n\
         the gate and the release pipeline (W8)\n\
         \x20 gates [--markdown] show every deferred gate and whether it is armed\n\
         \x20 gate <id>          run one gate, or report it as deferred and succeed.\n\
         \x20                    Exits with the command's own code, and reports the\n\
         \x20                    meaning that code has in ci/gates.toml.\n\
         \x20 coverage --check   compare line coverage against ci/coverage-floor.txt\n\
         \x20          --bump    raise the floor to the measured value\n\
         \x20 dist --target <t>  build and package one release artefact\n\
         \x20                    [--version <v>] [--out <dir>]\n\
         \x20 targets            the release matrix from ci/release.toml, as JSON for a\n\
         \x20        [--linux-only]  GitHub Actions strategy.matrix\n\
         \n\
         Exit codes: 0 ok \u{b7} 1 drift found by --check \u{b7} 2 invariant violated or bad \
         usage \u{b7} gate <id>: the gate command's own code"
    );
}

/// Repository root, derived from this crate's manifest rather than the current directory
/// so that every command behaves the same wherever it is invoked from.
pub fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/xtask when run through the alias.
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent().map(Path::to_path_buf).unwrap_or(here)
}

/// W8's modules take owned args; W2's `migrate` takes borrowed ones. Both exist.
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

fn load(root: &Path) -> Result<Registry, String> {
    let reg = Registry::load(&root.join(REGISTRY))?;
    validate::registry_internal(&reg)?;
    Ok(reg)
}

fn load_snapshot(root: &Path) -> Option<Result<Snapshot, String>> {
    let p = root.join(SNAPSHOT);
    if p.exists() {
        Some(Snapshot::load(&p))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------

/// The registry, validated against the snapshot, expanded into exactly the files that
/// `generated/` should hold. Separate from `codegen` so that both halves of the pair — the
/// write and the drift check — are driven from one answer, and so that a test can ask for
/// that answer without a process boundary.
fn codegen_files(root: &Path) -> Result<gen::Files, String> {
    let reg = load(root)?;
    match load_snapshot(root) {
        Some(snap) => {
            validate::against_snapshot(&reg, &snap?, false)?;
        }
        None => {
            return Err(format!(
                "FATAL: {SNAPSHOT} does not exist, so append-only cannot be enforced.\n\
                 \x20      Run `cargo xtask migrate --name initial_schema` to create it."
            ))
        }
    }
    Ok(gen::all(&reg))
}

/// `--check` regenerates into a temporary location and diffs. It must never write into the
/// working tree: a check that fixes the problem it is checking for always passes (R-2).
///
/// Returns the temporary directory it regenerated into — the error message names it so the
/// developer can diff against it — and one line per piece of drift. Empty means clean, and
/// that is the gate's verdict, returned rather than only printed so that a test can assert
/// on it.
fn codegen_drift(root: &Path, files: &gen::Files) -> Result<(PathBuf, Vec<String>), String> {
    let tmp = check_tmp_dir("codegen");
    let _ = std::fs::remove_dir_all(&tmp);
    for (path, body) in files {
        let full = tmp.join(path);
        std::fs::create_dir_all(full.parent().unwrap())
            .map_err(|e| format!("{}: {e}", full.display()))?;
        std::fs::write(&full, body).map_err(|e| format!("{}: {e}", full.display()))?;
    }

    let on_disk = read_tree(&root.join(GENERATED), root);
    let mut drift: Vec<String> = Vec::new();
    for (path, want) in files {
        match on_disk.get(path) {
            None => drift.push(format!("  missing from the tree: {path}")),
            Some(have) if have != want => {
                drift.push(format!("  differs: {path}{}", first_difference(have, want)));
            }
            Some(_) => {}
        }
    }
    // Orphans are found by path, which is how the prune finds them too: the two halves of
    // the pair have to agree on what an orphan is, and `read_tree` cannot see a file that
    // is not valid UTF-8 — which, under `generated/`, cannot have come from an emitter.
    let mut orphans = list_files(&root.join(GENERATED), root);
    orphans.sort();
    for path in orphans {
        if !files.contains_key(&path) {
            drift.push(format!("  in the tree but not generated: {path}"));
        }
    }
    Ok((tmp, drift))
}

fn codegen(root: &Path, check: bool) -> Result<ExitCode, String> {
    let files = codegen_files(root)?;

    if !check {
        let pruned = write_generated(root, GENERATED, &files)?;
        println!("codegen: wrote {} files to {GENERATED}/", files.len());
        if !pruned.is_empty() {
            println!(
                "codegen: removed {} file{} {GENERATED}/ no longer holds:",
                pruned.len(),
                if pruned.len() == 1 { "" } else { "s" }
            );
            for path in &pruned {
                println!("  {path}");
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    let (tmp, drift) = codegen_drift(root, &files)?;

    if drift.is_empty() {
        let _ = std::fs::remove_dir_all(&tmp);
        println!(
            "codegen --check: {} generated files match metrics/registry.toml",
            files.len()
        );
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!(
            "codegen --check: generated output does not match metrics/registry.toml\n{}\n\n\
             Generated files are build artefacts. Edit metrics/registry.toml and run\n\
             `cargo xtask codegen`, then commit both. Regenerated into {} for the diff; the\n\
             working tree was not touched.",
            drift.join("\n"),
            tmp.display()
        );
        Ok(ExitCode::from(1))
    }
}

/// Writes `files` under `root`, then deletes everything else under `root/dir`.
///
/// The deletion is the point (N-15 / OQ-W4-4). `dir` is wholly owned by the generator —
/// every file under it carries a header saying so — and a file there that the generator no
/// longer emits is reported by `--check` as `in the tree but not generated`. Without the
/// prune, `codegen` exits 0 having produced a tree that the gate it is paired with rejects,
/// which is a developer running `codegen`, committing, and watching CI fail on files
/// `codegen` itself made stale.
///
/// Renumbering is what makes that routine rather than rare: the aggregate files are
/// numbered by the level's position, so inserting a `[[level]]` renames every file above it
/// and orphans one per level.
///
/// Returns the paths removed, repo-relative and sorted.
fn write_generated(root: &Path, dir: &str, files: &gen::Files) -> Result<Vec<String>, String> {
    for (path, body) in files {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap())
            .map_err(|e| format!("{}: {e}", full.display()))?;
        std::fs::write(&full, body).map_err(|e| format!("{}: {e}", full.display()))?;
    }

    let mut pruned: Vec<String> = Vec::new();
    for rel in list_files(&root.join(dir), root) {
        if files.contains_key(&rel) {
            continue;
        }
        let full = root.join(&rel);
        std::fs::remove_file(&full).map_err(|e| format!("{}: {e}", full.display()))?;
        pruned.push(rel);
    }
    pruned.sort();
    remove_empty_dirs(&root.join(dir));
    Ok(pruned)
}

/// Every file under `dir`, repo-relative. Listed by path rather than by content, unlike
/// `read_tree`: a file under `generated/` that is not valid UTF-8 cannot have come from an
/// emitter, so it is an orphan too, and `read_tree` cannot see it to say so.
///
/// A symlink counts as a file and is never followed. Nothing the generator writes is one,
/// and descending into one would take the prune outside the directory it owns.
fn list_files(dir: &Path, root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            match std::fs::symlink_metadata(&p) {
                Ok(md) if md.is_dir() => stack.push(p),
                Ok(_) => out.push(rel_path(&p, root)),
                Err(_) => {}
            }
        }
    }
    out
}

/// Bottom-up, so that pruning the last file out of `generated/<target>/` does not leave the
/// directory behind for the next `ls` to puzzle over. `dir` itself always stays.
fn remove_empty_dirs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if std::fs::symlink_metadata(&p).is_ok_and(|md| md.is_dir()) {
            remove_empty_dirs(&p);
            if std::fs::read_dir(&p).is_ok_and(|mut it| it.next().is_none()) {
                let _ = std::fs::remove_dir(&p);
            }
        }
    }
}

fn rel_path(p: &Path, root: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Where a `--check` regenerates to. Unique per call rather than per process: `cargo test`
/// runs cases concurrently in one process, so a pid-only path lets one case's cleanup land
/// in the middle of another case's write.
fn check_tmp_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "mw-{tag}-check-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

fn read_tree(dir: &Path, root: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(text) = std::fs::read_to_string(&p) {
                out.insert(rel_path(&p, root), text);
            }
        }
    }
    out
}

fn first_difference(have: &str, want: &str) -> String {
    for (i, (a, b)) in have.lines().zip(want.lines()).enumerate() {
        if a != b {
            return format!(
                "\n      line {}:\n        in the tree: {}\n        generated:   {}",
                i + 1,
                a.trim_end(),
                b.trim_end()
            );
        }
    }
    let (h, w) = (have.lines().count(), want.lines().count());
    if h != w {
        format!("\n      {h} lines in the tree, {w} generated")
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------

fn migrate_cmd(root: &Path, flags: &[&str]) -> Result<ExitCode, String> {
    let reg = load(root)?;
    let allow_breaking = flags.contains(&"--allow-breaking");
    let dry_run = flags.contains(&"--dry-run");
    let name = flag_value_str(flags, "--name");
    let stamp_override = flag_value_str(flags, "--stamp");

    let (changes, bootstrap, next_snapshot) = match load_snapshot(root) {
        None => {
            let _ = validate::against_snapshot(
                &reg,
                &Snapshot {
                    schema_version: 1,
                    entries: vec![],
                },
                allow_breaking,
            )?;
            (vec![], true, snapshot::from_registry(&reg))
        }
        Some(s) => {
            let snap = s?;
            let changes = validate::against_snapshot(&reg, &snap, allow_breaking)?;
            if changes.is_empty() {
                println!("migrate: registry and snapshot agree; nothing to emit");
                return Ok(ExitCode::SUCCESS);
            }
            (changes, false, snapshot::from_registry(&reg))
        }
    };

    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let stamp = stamp_override.unwrap_or_else(|| migrate::stamp(secs));
    let default_name = if bootstrap {
        "initial_schema".to_string()
    } else {
        derive_name(&changes)
    };
    let m = migrate::render(
        &reg,
        &changes,
        bootstrap,
        &stamp,
        &name.unwrap_or(default_name),
    );

    println!(
        "migrate: {} change(s)",
        changes.len().max(usize::from(bootstrap))
    );
    for c in &changes {
        println!(
            "  [{}] {}: {}",
            validate::class_label(c.class),
            c.key,
            c.detail
        );
    }

    if dry_run {
        println!("\n--dry-run: would write {MIGRATIONS}/{}\n", m.filename);
        println!("{}", m.body);
        return Ok(ExitCode::SUCCESS);
    }

    let dir = root.join(MIGRATIONS);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(&m.filename);
    std::fs::write(&path, &m.body).map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::write(root.join(SNAPSHOT), snapshot::render(&next_snapshot))
        .map_err(|e| format!("{SNAPSHOT}: {e}"))?;
    println!(
        "migrate: wrote {MIGRATIONS}/{} and updated {SNAPSHOT}",
        m.filename
    );
    println!("migrate: now run `cargo xtask codegen` and commit generated/");
    Ok(ExitCode::SUCCESS)
}

/// `migrate` parses its own borrowed flags; the owned-arg `flag_value` above is W8's.
fn flag_value_str(flags: &[&str], name: &str) -> Option<String> {
    let i = flags.iter().position(|f| *f == name)?;
    flags.get(i + 1).map(|s| (*s).to_string())
}

fn derive_name(changes: &[snapshot::Change]) -> String {
    use snapshot::Class;
    if changes.iter().any(|c| c.class.breaking()) {
        return "breaking_schema_change".into();
    }
    let adds = changes
        .iter()
        .filter(|c| c.class == Class::AddColumn)
        .count();
    let deps = changes
        .iter()
        .filter(|c| c.class == Class::Deprecate)
        .count();
    // "entries", not "metrics": a dimension can reach both of these now (N-14), and the
    // name lands in a permanent migration filename.
    let first = |class: snapshot::Class| {
        changes
            .iter()
            .find(|c| c.class == class)
            .map(|c| c.key.clone())
            .unwrap_or_default()
    };
    match (adds, deps) {
        (0, 0) => "registry_metadata".into(),
        (1, 0) => format!("add_{}", first(Class::AddColumn)),
        (n, 0) => format!("add_{n}_entries"),
        (0, 1) => format!("deprecate_{}", first(Class::Deprecate)),
        (0, n) => format!("deprecate_{n}_entries"),
        (a, d) => format!("add_{a}_deprecate_{d}"),
    }
}

// ---------------------------------------------------------------------------

fn validate_cmd(root: &Path) -> Result<ExitCode, String> {
    let reg = load(root)?;
    match load_snapshot(root) {
        Some(s) => {
            let changes = validate::against_snapshot(&reg, &s?, false)?;
            if changes.is_empty() {
                println!("validate: registry, snapshot and budget all check out");
            } else {
                println!(
                    "validate: invariants hold; {} pending change(s):",
                    changes.len()
                );
                for c in &changes {
                    println!(
                        "  [{}] {}: {}",
                        validate::class_label(c.class),
                        c.key,
                        c.detail
                    );
                }
            }
        }
        None => println!("validate: registry is internally consistent ({SNAPSHOT} absent)"),
    }
    Ok(ExitCode::SUCCESS)
}

fn budget(root: &Path) -> Result<ExitCode, String> {
    let reg = load(root)?;
    let snap = match load_snapshot(root) {
        Some(s) => Some(s?),
        None => None,
    };
    let b = validate::budget(&reg, snap.as_ref());
    println!("Column budget — counted against allocated identifiers, not live ones.");
    println!("A dropped PostgreSQL column keeps its attribute number forever, and a");
    println!("compressed hypertable's shadow table mirrors every column including the");
    println!(
        "dropped ones and adds {} of its own.\n",
        validate::COMPRESSION_METADATA_COLUMNS
    );
    println!("  raw hypertable");
    println!("    allocated identifiers   {:>6}", b.allocated_columns);
    println!("    live columns            {:>6}", b.live_columns);
    println!("    effective ceiling       {:>6}  (PostgreSQL's {} minus {} compression metadata columns)",
             validate::RAW_COLUMN_CEILING, validate::COLUMN_LIMIT, validate::COMPRESSION_METADATA_COLUMNS);
    println!(
        "    headroom                {:>6}\n",
        validate::RAW_COLUMN_CEILING.saturating_sub(b.allocated_columns)
    );
    println!("  aggregates                metrics  expressions  ceiling  headroom");
    for (name, metrics, exprs) in &b.per_level {
        println!(
            "    {:<22} {:>7} {:>12} {:>8} {:>9}",
            format!("{}_{}", gen::RAW_TABLE, name),
            metrics,
            exprs,
            validate::AGG_EXPRESSION_CEILING,
            validate::AGG_EXPRESSION_CEILING.saturating_sub(*exprs)
        );
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// W9: the Go-to-Rust data migration emitter.
// ---------------------------------------------------------------------------

/// Emits `ci/db/migrate-from-go/generated/`. Deliberately a separate subcommand from
/// `codegen` rather than another entry in `gen::all`, because its second input is not the
/// registry: `go-aggregate-views.toml` describes a *frozen* tree at a named commit, and
/// `codegen --check` is the gate that says "generated/ matches metrics/registry.toml". It
/// should keep saying exactly that.
fn legacy_import(root: &Path, flags: &[&str]) -> Result<ExitCode, String> {
    let reg = load(root)?;
    let go = legacy::GoViews::load(&root.join(legacy::GO_VIEWS))?;
    let files = legacy::all(&reg, &go);

    if !flags.contains(&"--check") {
        for (path, body) in &files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap())
                .map_err(|e| format!("{}: {e}", full.display()))?;
            std::fs::write(&full, body).map_err(|e| format!("{}: {e}", full.display()))?;
        }
        println!(
            "legacy-import: wrote {} files to {} (registry {} metrics, {} at {})",
            files.len(),
            legacy::OUT_DIR,
            reg.live_metrics().len(),
            go.source_file,
            &go.source_commit[..12],
        );
        return Ok(ExitCode::SUCCESS);
    }

    // Same discipline as `codegen --check`: never write into the working tree, because a
    // check that fixes the problem it checks for always passes (R-2).
    let on_disk = read_tree(&root.join("ci/db/migrate-from-go/generated"), root);
    let mut drift: Vec<String> = Vec::new();
    for (path, want) in &files {
        match on_disk.get(path) {
            None => drift.push(format!("  missing from the tree: {path}")),
            Some(have) if have != want => {
                drift.push(format!("  differs: {path}{}", first_difference(have, want)));
            }
            Some(_) => {}
        }
    }
    for path in on_disk.keys() {
        if !files.contains_key(path) {
            drift.push(format!("  in the tree but not generated: {path}"));
        }
    }
    if drift.is_empty() {
        println!(
            "legacy-import --check: {} files match the registry and the Go view inventory",
            files.len()
        );
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!(
            "legacy-import --check: emitted output does not match its inputs\n{}\n\n\
             Edit metrics/registry.toml, or re-run\n\
             `ci/db/migrate-from-go/extract-go-views.sh <remoteweather>`, then run\n\
             `cargo xtask legacy-import` and commit both.",
            drift.join("\n")
        );
        Ok(ExitCode::from(1))
    }
}

/// N-15 / OQ-W4-4: `codegen` owns `generated/`, so it has to remove what it no longer
/// writes. The cases below run the real `codegen` against a scratch repository root
/// holding a copy of `metrics/`, because the claim being tested is about a directory on
/// disk and nothing smaller reaches it.
#[cfg(test)]
mod prune {
    use super::{codegen, codegen_drift, codegen_files, repo_root, write_generated};
    use super::{GENERATED, REGISTRY};
    use std::path::{Path, PathBuf};

    /// A repository root with nothing in it but `metrics/`. The real tree is never
    /// written to: these tests add a level, regenerate, and remove it again, which is
    /// exactly the sequence that must not be performed where someone could commit the
    /// intermediate state.
    fn scratch_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mw-prune-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("metrics")).expect("scratch root");
        for f in ["registry.toml", "snapshot.toml"] {
            std::fs::copy(
                repo_root().join("metrics").join(f),
                root.join("metrics").join(f),
            )
            .expect("metrics/ is copied");
        }
        root
    }

    fn files_under(root: &Path, dir: &str) -> Vec<String> {
        let mut v = super::list_files(&root.join(dir), root);
        v.sort();
        v
    }

    /// The second half of the done-criterion: `codegen` followed immediately by
    /// `codegen --check` exits 0. Asserted on the drift list rather than on the exit
    /// code, because `ExitCode` is opaque and a drift line is the thing worth reading.
    fn assert_no_drift(root: &Path, when: &str) {
        let files = codegen_files(root).expect("the registry is valid");
        let (tmp, drift) = codegen_drift(root, &files).expect("the check runs");
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(
            drift.is_empty(),
            "`codegen --check` rejects the tree {when}:\n{}",
            drift.join("\n")
        );
    }

    /// Inserts a 2-minute level between `1m` and `5m` the way a developer would: the
    /// block itself, the level above it re-pointed at it, and `[defaults].levels` plus
    /// every metric's membership extended, since membership must stay contiguous from the
    /// first level.
    fn with_a_two_minute_level(registry: &str) -> String {
        let was = "[[level]]\nname = \"5m\"\nbucket = \"5 minutes\"\nsource = \"1m\"\n";
        let now = "[[level]]\nname = \"2m\"\nbucket = \"2 minutes\"\nsource = \"1m\"\n\
                   materialized_only = true\n\n\
                   [[level]]\nname = \"5m\"\nbucket = \"5 minutes\"\nsource = \"2m\"\n";
        assert!(
            registry.contains(was),
            "the 5-minute level is declared the way this edit expects"
        );
        registry.replace(was, now).replace(
            "levels = [\"1m\", \"5m\", \"1h\", \"1d\"]",
            "levels = [\"1m\", \"2m\", \"5m\", \"1h\", \"1d\"]",
        )
    }

    /// The regression, run as the round trip the plan asks for. The aggregate files are
    /// numbered by the level's position, so inserting a `[[level]]` renames every file
    /// above it: before the prune, `codegen` wrote the new names, left the old ones,
    /// exited 0, and handed back a tree that `codegen --check` rejects with one
    /// `in the tree but not generated` line per renumbered level.
    #[test]
    fn adding_and_removing_a_level_leaves_no_orphan() {
        let root = scratch_root("roundtrip");
        let original = std::fs::read_to_string(root.join(REGISTRY)).expect("the registry");

        codegen(&root, false).expect("the baseline generates");
        let baseline = files_under(&root, GENERATED);
        let five = "generated/sql/040_observations_5m.sql".to_string();
        assert!(
            baseline.contains(&five),
            "the 5-minute level is the fourth file; the renumbering case depends on it"
        );
        assert_no_drift(&root, "as committed");

        std::fs::write(root.join(REGISTRY), with_a_two_minute_level(&original)).unwrap();
        codegen(&root, false).expect("the added level generates");
        let added = files_under(&root, GENERATED);
        assert_eq!(
            added.len(),
            baseline.len() + 1,
            "one level added is one file added and no file orphaned: {added:#?}"
        );
        assert!(added.contains(&"generated/sql/040_observations_2m.sql".to_string()));
        assert!(
            !added.contains(&five),
            "{five} is the 5-minute level's *old* number and is now an orphan"
        );
        assert_no_drift(&root, "with a 2-minute level added");

        std::fs::write(root.join(REGISTRY), &original).unwrap();
        codegen(&root, false).expect("the removed level regenerates");
        assert_eq!(
            files_under(&root, GENERATED),
            baseline,
            "removing the level must restore the committed set exactly"
        );
        assert_no_drift(&root, "with the 2-minute level removed again");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `generated/` is the only directory the prune owns, and the directory tree inside it
    /// is the generator's too. These are the claims the round trip above cannot make,
    /// because the generator emits no nested directory, no file outside `generated/` and
    /// nothing that is not valid UTF-8.
    #[test]
    fn the_prune_owns_the_generated_directory_and_nothing_else() {
        let root = scratch_root("boundaries");
        let files = codegen_files(&root).expect("the registry is valid");
        write_generated(&root, GENERATED, &files).expect("the baseline is written");

        // An orphan two directories down, a sibling of `generated/` that is not the
        // generator's, and an orphan `read_tree` cannot read — a non-UTF-8 file under
        // `generated/` cannot have come from an emitter, so it is stale by construction.
        std::fs::create_dir_all(root.join("generated/stale/deeper")).unwrap();
        std::fs::write(root.join("generated/stale/deeper/orphan.sql"), "-- stale\n").unwrap();
        std::fs::write(root.join("generated/sql/not-utf8.bin"), [0xff_u8, 0xfe]).unwrap();
        std::fs::write(root.join("metrics/keep.toml"), "# not the generator's\n").unwrap();

        // The check and the prune have to agree on what an orphan is. The binary file is
        // where a content-based answer and a path-based one come apart: `read_tree` cannot
        // read it, so a check that looked only at content would call this tree clean and
        // then watch `codegen` delete a file from it.
        let (tmp, drift) = codegen_drift(&root, &files).expect("the check runs");
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(
            drift,
            vec![
                "  in the tree but not generated: generated/sql/not-utf8.bin".to_string(),
                "  in the tree but not generated: generated/stale/deeper/orphan.sql".to_string(),
            ],
        );

        let pruned = write_generated(&root, GENERATED, &files).expect("the prune runs");
        assert_eq!(
            pruned,
            vec![
                "generated/sql/not-utf8.bin".to_string(),
                "generated/stale/deeper/orphan.sql".to_string(),
            ],
            "every file under generated/ that the generator did not write, and only those"
        );
        assert!(
            !root.join("generated/stale").exists(),
            "a directory left empty by the prune goes with its files"
        );
        assert!(
            root.join("metrics/keep.toml").exists(),
            "the prune reached outside generated/"
        );
        assert_eq!(files_under(&root, GENERATED).len(), files.len());

        let _ = std::fs::remove_dir_all(&root);
    }
}

/// The generated Rust is compiled here rather than merely written out. `metrics.rs`
/// and `aggregates.rs` depend on nothing but each other, so including them in a test
/// module turns "the generator emits plausible text" into "the generator emits Rust
/// that type-checks". `reading.rs` is not included: it needs `chrono`, and W3 owns
/// which crate the application types live in (R-4).
#[cfg(test)]
#[allow(dead_code, clippy::enum_variant_names)]
mod generated_compiles {
    pub mod metrics {
        include!("../../generated/rust/metrics.rs");
    }
    pub mod aggregates {
        include!("../../generated/rust/aggregates.rs");
    }

    use super::{load, repo_root};

    #[test]
    fn metadata_table_matches_the_registry() {
        let reg = load(&repo_root()).expect("registry is valid");
        assert_eq!(metrics::METRICS.len(), reg.live_metrics().len());
        for (meta, m) in metrics::METRICS.iter().zip(reg.live_metrics()) {
            assert_eq!(meta.name, m.key);
            assert_eq!(meta.column, m.column);
            assert_eq!(meta.field_number, m.field_number);
        }
    }

    #[test]
    fn no_aggregate_column_holds_a_finished_average() {
        for level in aggregates::LEVELS {
            for c in level.columns {
                assert!(
                    !c.column.ends_with("_avg") && !c.column.ends_with("_mean"),
                    "{} looks like a finished average (D-12)",
                    c.column
                );
            }
        }
    }

    #[test]
    fn every_level_but_the_first_reads_the_level_below_it() {
        let levels = aggregates::LEVELS;
        assert_eq!(levels[0].source, "observations");
        for pair in levels.windows(2) {
            assert_eq!(
                pair[1].source, pair[0].view,
                "a level must read the level below it, never raw (D-11)"
            );
        }
    }

    /// Scans the emitted SQL rather than the model, because the model being right and
    /// the template being right are two different claims. Comment lines are skipped:
    /// the generated headers quote `avg(avg(x))` and `sum(real)` on purpose.
    #[test]
    fn the_generated_aggregate_sql_contains_no_naive_form() {
        let reg = load(&repo_root()).expect("registry is valid");
        let files = super::gen::all(&reg);
        for (path, body) in &files {
            // Every level file, matched on the generator's own naming rather than on a
            // hard-coded list: the filter used to read `observations_1`, which silently
            // stopped covering the 5-minute level the moment it was added.
            if !path.starts_with("generated/sql/")
                || !path.contains("_observations_")
                || path.contains("090_")
            {
                continue;
            }
            for (i, line) in body.lines().enumerate() {
                let t = line.trim_start();
                if t.starts_with("--") {
                    continue;
                }
                assert!(
                    !t.contains("avg("),
                    "{path}:{}: an averaging call in generated aggregate SQL (D-12): {t}",
                    i + 1
                );
                if let Some(rest) = t.strip_prefix("sum(") {
                    // Either a widened cast from raw, or a roll-up of a column that is
                    // already a widened accumulator.
                    assert!(
                        rest.contains("::double precision")
                            || rest.starts_with(|_c: char| false)
                            || rest
                                .split(')')
                                .next()
                                .is_some_and(|c| { c.ends_with("_sum") || c.ends_with("_total") }),
                        "{path}:{}: a sum that is neither widened nor a roll-up (D-14): {t}",
                        i + 1
                    );
                }
                assert!(
                    !t.contains("count(") || t.contains("nullif("),
                    "{path}:{}: a count that is not nulled at zero (D-16): {t}",
                    i + 1
                );
            }
        }
    }

    /// `sum(bigint)` returns `numeric`, so a roll-up without a cast gives every level
    /// above the first a numeric count while this struct — and W3's query layer with it —
    /// says bigint. `mw_mean` is `(double precision, bigint)` and numeric does not
    /// implicitly narrow, so the symptom was three of `/span`'s four bands failing to
    /// resolve the finaliser at all. Found by running the query; kept by this test and by
    /// ci/db/schema-assertions.sql A-1.
    #[test]
    fn a_rolled_up_count_is_cast_back_to_bigint() {
        let reg = load(&repo_root()).expect("registry is valid");
        let mut checked = 0;
        for level in &reg.levels {
            if level.source == "raw" {
                continue;
            }
            for (_, c) in reg.level_columns(&level.name) {
                if !c.name.ends_with("_count") {
                    continue;
                }
                assert_eq!(
                    c.sql_type, "bigint",
                    "{}.{} is declared {}",
                    level.name, c.name, c.sql_type
                );
                assert!(
                    c.from_level.ends_with("::bigint"),
                    "{}.{} rolls up as `{}`, which is numeric — mw_mean cannot be called \
                     on it and sqlx will decode it as the wrong type",
                    level.name,
                    c.name,
                    c.from_level
                );
                checked += 1;
            }
        }
        assert!(
            checked > 0,
            "no count columns found, so this test proved nothing"
        );
    }

    /// OQ-W1-2 option (c), W3 assumption 18: a bucket with no non-NULL `winddir` must
    /// finalise to `0`, not to `null`. `windd` is a non-optional Swift `Double` and a
    /// missing key fails the decode of the entire `/span` response, so `null` here is a
    /// contract change rather than a tidier answer. The asymmetry with `mw_mean`, which
    /// stays NULL, is the decision — both halves are asserted.
    #[test]
    fn the_circular_mean_finaliser_returns_zero_for_an_empty_bucket() {
        let sql = super::gen::finalisers();
        let circular = sql
            .split("CREATE OR REPLACE FUNCTION mw_circular_mean")
            .nth(1)
            .expect("mw_circular_mean is generated");
        assert!(
            circular.contains("THEN 0"),
            "mw_circular_mean does not return 0 for an empty bucket (W1-BUG-1)"
        );
        assert!(
            !circular.contains("THEN NULL"),
            "mw_circular_mean returns NULL for an empty bucket, which is OQ-W1-2 option \
             (b) — a contract change, not option (c)"
        );

        let mean = sql
            .split("CREATE OR REPLACE FUNCTION mw_mean")
            .nth(1)
            .expect("mw_mean is generated");
        let mean_body = mean.split("$$").nth(1).expect("mw_mean has a body");
        assert!(
            mean_body.contains("THEN NULL"),
            "mw_mean must stay NULL for an empty bucket; only the direction is zero-filled"
        );
    }

    /// The finalisers are the one piece of generated SQL whose *body* can change without
    /// any identifier changing, so no snapshot diff is produced and nothing else would
    /// carry the change into a deployed database. Before this, they were emitted only by
    /// the bootstrap migration.
    #[test]
    fn every_migration_reasserts_the_finalisers() {
        let reg = load(&repo_root()).expect("registry is valid");
        let m = super::migrate::render(&reg, &[], false, "20260101000000", "no_op");
        assert!(
            m.body
                .contains("CREATE OR REPLACE FUNCTION mw_circular_mean"),
            "a migration that changes only a finaliser would reach generated/ and never \
             reach a database"
        );
        assert!(m.body.contains("CREATE OR REPLACE FUNCTION mw_mean"));
    }

    /// D-89 / G-3. Scans the emitted proto rather than the model, for the reason the
    /// aggregate-SQL test gives: the model being right and the template being right are two
    /// different claims, and this decision was made by three documents and implemented by
    /// none of them for the whole design programme.
    #[test]
    fn every_metric_field_in_the_proto_carries_field_presence() {
        let reg = load(&repo_root()).expect("registry is valid");
        let files = super::gen::all(&reg);
        let proto = files
            .get("generated/proto/reading.proto")
            .expect("the proto is generated");

        for m in reg.live_metrics() {
            let want = format!(
                "optional {} {} = {};",
                m.proto_type, m.proto_name, m.field_number
            );
            assert!(
                proto.contains(&want),
                "metric `{}` must be `optional` (D-89): without presence, an unreported metric \
                 decodes as 0.0 and is stored instead of NULL, silently and permanently",
                m.key
            );
        }
    }

    /// The converse, and the half that would rot first: a future generator change that
    /// blanket-prefixed every field would break nothing visible and would put presence on
    /// fields that are present by construction.
    #[test]
    fn no_dimension_field_in_the_proto_carries_field_presence() {
        let reg = load(&repo_root()).expect("registry is valid");
        let files = super::gen::all(&reg);
        let proto = files
            .get("generated/proto/reading.proto")
            .expect("the proto is generated");

        // Presence is asserted over *every* dimension, retired ones included: a retired
        // field is absent from the proto, so the assertion holds trivially, and a
        // generator that blanket-prefixed the message would still be caught.
        for d in &reg.dimensions {
            let bad = format!("optional {} {} =", d.proto_type, d.proto_name);
            assert!(
                !proto.contains(&bad),
                "dimension `{}` must not be `optional`: the time and partition dimensions are \
                 present by construction, the attribute dimensions are being removed entirely \
                 (OQ-W4-2), and a `metadata` dimension is wire metadata rather than a \
                 measurement (N-14)",
                d.key
            );
        }
        // Emission is asserted over the live ones only. A retired dimension leaves the
        // message and keeps its number `reserved` (D-21, N-14), so requiring it here
        // would fail the suite the first time anything used that half of M1-2.
        for d in reg.live_dimensions() {
            let want = format!("{} {} = {};", d.proto_type, d.proto_name, d.field_number);
            assert!(proto.contains(&want), "dimension `{}` is emitted", d.key);
        }
        for d in reg.dimensions.iter().filter(|d| d.deprecated) {
            let gone = format!("{} {} = {};", d.proto_type, d.proto_name, d.field_number);
            assert!(!proto.contains(&gone), "dimension `{}` is retired", d.key);
            assert!(
                proto.contains(&format!("reserved {}", d.field_number))
                    || proto.contains(&format!(", {};", d.field_number))
                    || proto.contains(&format!(", {},", d.field_number)),
                "retired dimension `{}` must keep protobuf field {} reserved",
                d.key,
                d.field_number
            );
        }
    }

    /// `optional` is only wire-compatible because it changes no field number. This asserts
    /// the property directly rather than trusting the claim in the generated header.
    #[test]
    fn field_presence_renumbers_nothing() {
        let reg = load(&repo_root()).expect("registry is valid");
        let files = super::gen::all(&reg);
        let proto = files
            .get("generated/proto/reading.proto")
            .expect("the proto is generated");

        for m in reg.live_metrics() {
            let assignments = proto
                .matches(&format!(" {} = {};", m.proto_name, m.field_number))
                .count();
            assert_eq!(
                assignments, 1,
                "`{}` must be assigned field number {} exactly once",
                m.proto_name, m.field_number
            );
        }
    }

    #[test]
    fn mean_is_stored_as_composable_state_in_a_wider_type() {
        let one_minute = aggregates::LEVELS[0];
        let sums: Vec<_> = one_minute
            .columns
            .iter()
            .filter(|c| c.strategy == aggregates::Strategy::Mean && c.column.ends_with("_sum"))
            .collect();
        assert!(!sums.is_empty());
        for c in sums {
            assert_eq!(
                c.sql_type, "double precision",
                "{} accumulates a sum in the column's own type (D-14)",
                c.column
            );
        }
    }
}
