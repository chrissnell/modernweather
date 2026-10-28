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
//!   1  `--check` found drift, or a gate failed
//!   2  an invariant was violated, or usage was wrong

mod coverage;
mod dist;
mod gates;
mod gen;
mod migrate;
mod registry;
mod snapshot;
mod validate;

use registry::Registry;
use snapshot::Snapshot;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const REGISTRY: &str = "metrics/registry.toml";
const SNAPSHOT: &str = "metrics/snapshot.toml";
const MIGRATIONS: &str = "migrations";

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
        "budget" => budget(&root),
        "validate" => validate_cmd(&root),
        "gates" => gates::list(&owned).map(|()| ExitCode::SUCCESS),
        "gate" => gates::run(&owned).map(|()| ExitCode::SUCCESS),
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
         \x20 codegen            regenerate generated/ from metrics/registry.toml\n\
         \x20 codegen --check    regenerate into a temporary directory and diff; never\n\
         \x20                    writes into the working tree. Exit 1 on drift.\n\
         \x20 migrate            diff the registry against metrics/snapshot.toml and emit a\n\
         \x20                    timestamped sqlx migration, then update the snapshot\n\
         \x20     --name <s>         migration name (default: derived from the changes)\n\
         \x20     --allow-breaking   permit column renames, type changes and strategy\n\
         \x20                        changes. Loud on purpose; read the migration.\n\
         \x20     --dry-run          print what would be written, write nothing\n\
         \x20 validate           check every invariant and exit\n\
         \x20 budget             print the column budget against the ceiling\n\
         \n\
         the gate and the release pipeline (W8)\n\
         \x20 gates [--markdown] show every deferred gate and whether it is armed\n\
         \x20 gate <id>          run one gate, or report it as deferred and succeed\n\
         \x20 coverage --check   compare line coverage against ci/coverage-floor.txt\n\
         \x20          --bump    raise the floor to the measured value\n\
         \x20 dist --target <t>  build and package one release artefact\n\
         \x20                    [--version <v>] [--out <dir>]\n\
         \x20 targets            the release matrix from ci/release.toml, as JSON for a\n\
         \x20        [--linux-only]  GitHub Actions strategy.matrix\n\
         \n\
         Exit codes: 0 ok \u{b7} 1 drift found by --check \u{b7} 2 invariant violated or bad usage"
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

fn codegen(root: &Path, check: bool) -> Result<ExitCode, String> {
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

    let files = gen::all(&reg);

    if !check {
        for (path, body) in &files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap())
                .map_err(|e| format!("{}: {e}", full.display()))?;
            std::fs::write(&full, body).map_err(|e| format!("{}: {e}", full.display()))?;
        }
        println!("codegen: wrote {} files to generated/", files.len());
        return Ok(ExitCode::SUCCESS);
    }

    // --check regenerates into a temporary location and diffs. It must never write
    // into the working tree: a check that fixes the problem it is checking for
    // always passes (R-2).
    let tmp = std::env::temp_dir().join(format!("mw-codegen-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    for (path, body) in &files {
        let full = tmp.join(path);
        std::fs::create_dir_all(full.parent().unwrap())
            .map_err(|e| format!("{}: {e}", full.display()))?;
        std::fs::write(&full, body).map_err(|e| format!("{}: {e}", full.display()))?;
    }

    let on_disk = read_tree(&root.join("generated"), root);
    let regenerated: BTreeMap<String, String> = files;

    let mut drift: Vec<String> = Vec::new();
    for (path, want) in &regenerated {
        match on_disk.get(path) {
            None => drift.push(format!("  missing from the tree: {path}")),
            Some(have) if have != want => {
                drift.push(format!("  differs: {path}{}", first_difference(have, want)));
            }
            Some(_) => {}
        }
    }
    for path in on_disk.keys() {
        if !regenerated.contains_key(path) {
            drift.push(format!("  in the tree but not generated: {path}"));
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);

    if drift.is_empty() {
        println!(
            "codegen --check: {} generated files match metrics/registry.toml",
            regenerated.len()
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
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, text);
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
    match (adds, deps) {
        (0, 0) => "registry_metadata".into(),
        (1, 0) => format!("add_{}", changes[0].key),
        (n, 0) => format!("add_{n}_metrics"),
        (0, n) => format!("deprecate_{n}_metrics"),
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
            if !path.contains("observations_1") {
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
