//! The committed schema snapshot: the record of every identifier that has ever been
//! allocated, and the only thing that makes "append-only" enforceable.
//!
//! The registry alone cannot enforce it — a human editing TOML can rename anything.
//! The generator diffs the registry against this file, which is written only by
//! `cargo xtask migrate`, and refuses the changes §2 of docs/metric-registry.md calls
//! fatal.

use crate::registry::{agg_columns_for, Level, MeanRepr, Registry, SqlType, Strategy};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub schema_version: u32,
    /// The aggregate hierarchy as last migrated (N-13). Absent from a snapshot written
    /// before M1-6, and `affected_levels` reads an absent section as "no previous
    /// definition to compare against", which rebuilds everything.
    #[serde(default, rename = "level")]
    pub levels: Vec<AllocatedLevel>,
    #[serde(default, rename = "allocated")]
    pub entries: Vec<Allocated>,
}

/// A level's own definition: everything about `CREATE MATERIALIZED VIEW` that is not
/// the column list. Recorded because the column list alone does not determine the
/// view — inserting the 5-minute level changed nothing about the hourly level's columns
/// and changed the relation it reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocatedLevel {
    pub name: String,
    pub bucket: String,
    pub source: String,
    pub materialized_only: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocated {
    pub kind: String,
    pub key: String,
    pub column: String,
    pub sql_type: String,
    pub field_number: u32,
    pub proto_type: String,
    #[serde(default)]
    pub aggregates: Vec<String>,
    #[serde(default)]
    pub mean_representation: Option<String>,
    #[serde(default)]
    pub levels: Vec<String>,
    #[serde(default)]
    pub deprecated: bool,
    pub introduced_in: String,
}

/// A dimension that is a column of the raw hypertable.
pub const DIMENSION: &str = "dimension";
/// A dimension that is not (N-14). A distinct `kind` rather than a new field, so that
/// `metrics/snapshot.toml` needs no format change: every entry it already holds is a
/// column, and this value can only appear on an entry written after M1-2.
pub const METADATA_DIMENSION: &str = "metadata_dimension";

/// Whether this entry is a dimension of either sort.
pub fn is_dimension(kind: &str) -> bool {
    kind == DIMENSION || kind == METADATA_DIMENSION
}

impl Snapshot {
    pub fn load(path: &Path) -> Result<Snapshot, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn by_key(&self) -> BTreeMap<&str, &Allocated> {
        self.entries.iter().map(|e| (e.key.as_str(), e)).collect()
    }
}

/// Build the snapshot that *would* be written for this registry.
pub fn from_registry(reg: &Registry) -> Vec<Allocated> {
    let mut out: Vec<Allocated> = Vec::new();
    for d in &reg.dimensions {
        out.push(Allocated {
            // `kind` is what makes a role change visible to the diff. The snapshot does
            // not record a role, and adding one would mean a format change and a
            // regeneration of all 216 committed entries — but `kind` is already written
            // for every one of them, and every existing dimension is a column. So a
            // dimension that stops being a column changes kind, and the diff below
            // classifies that rather than seeing no change at all.
            kind: if d.role.stored() {
                DIMENSION.into()
            } else {
                METADATA_DIMENSION.into()
            },
            key: d.key.clone(),
            column: d.column.clone(),
            sql_type: d.sql_type.ddl().into(),
            field_number: d.field_number,
            proto_type: d.proto_type.clone(),
            aggregates: vec![],
            mean_representation: None,
            levels: vec![],
            // Read from the registry rather than hard-coded false. That constant was
            // the one-way door: with it, no `[[dimension]]` could ever be retired,
            // because the only escape from the disappearance FATAL is
            // `deprecated = true` (N-14). Validation gates which roles may use it.
            deprecated: d.deprecated,
            introduced_in: d.introduced_in.clone(),
        });
    }
    for m in reg.metrics_ordered() {
        out.push(Allocated {
            kind: "metric".into(),
            key: m.key.clone(),
            column: m.column.clone(),
            sql_type: m.sql_type.ddl().into(),
            field_number: m.field_number,
            proto_type: m.proto_type.clone(),
            aggregates: m.aggregates.iter().map(|s| s.name().to_string()).collect(),
            mean_representation: Some(m.mean_repr(&reg.defaults).name().to_string()),
            levels: m.levels(&reg.defaults).to_vec(),
            deprecated: m.deprecated,
            introduced_in: m.introduced_in.clone(),
        });
    }
    out
}

/// The level hierarchy as the registry declares it today.
pub fn levels_from_registry(reg: &Registry) -> Vec<AllocatedLevel> {
    reg.levels.iter().map(AllocatedLevel::from_level).collect()
}

impl AllocatedLevel {
    fn from_level(l: &Level) -> AllocatedLevel {
        AllocatedLevel {
            name: l.name.clone(),
            bucket: l.bucket.clone(),
            source: l.source.clone(),
            materialized_only: l.materialized_only,
        }
    }
}

/// One aggregate level reduced to the text that determines it: its own definition, then
/// every aggregate column with the expression that produces it.
///
/// Not the generated SQL. `gen::aggregate_definition` carries a header whose measured
/// numbers and comments would make two identical views compare unequal the moment a
/// comment is reworded, and it needs a `Registry` on both sides — which the previous
/// side is not. This is the same information with nothing in it that is not the view.
///
/// An entry whose strategy, SQL type or mean representation this build cannot parse is
/// rendered verbatim rather than skipped: two snapshots that spell it the same way still
/// compare equal, and a change to it still shows up as a change.
fn rendered_level(
    lvl: &AllocatedLevel,
    entries: &[Allocated],
    time_column: &str,
    partition_column: &str,
) -> String {
    let from_raw = lvl.source == "raw";
    // The two dimension columns are in the header because `gen::aggregate_sql` interpolates
    // them into every level — the time column as the first level's `time_bucket` argument,
    // the partition column into the SELECT and the GROUP BY. Without them, renaming either
    // one rewrote all four generated views and returned no level to rebuild: PostgreSQL
    // propagates RENAME COLUMN into a dependent view's body but not into its output column
    // names, so a migrated database ended up with a view whose body and whose output
    // disagreed, and a fresh install did not. That is D-8's drift, from the tool that
    // exists to prevent it. Reachable only behind --allow-breaking, and two lines to close.
    let mut s = format!(
        "{} bucket={:?} source={:?} materialized_only={} time={:?} partition={:?}\n",
        lvl.name, lvl.bucket, lvl.source, lvl.materialized_only, time_column, partition_column
    );
    let mut carried: Vec<&Allocated> = entries
        .iter()
        .filter(|e| e.kind == "metric" && !e.deprecated && !e.aggregates.is_empty())
        .filter(|e| e.levels.iter().any(|l| l == &lvl.name))
        .collect();
    carried.sort_by_key(|e| e.field_number);
    for e in carried {
        let ty = SqlType::from_ddl(&e.sql_type);
        let strategies: Option<Vec<Strategy>> = e
            .aggregates
            .iter()
            .map(|a| Strategy::from_name(a))
            .collect();
        let repr = e
            .mean_representation
            .as_deref()
            .map_or(Some(MeanRepr::SumCount), MeanRepr::from_name);
        match (ty, strategies, repr) {
            (Some(ty), Some(strategies), Some(repr)) => {
                for c in agg_columns_for(&e.column, ty, &strategies, repr) {
                    let _ = writeln!(
                        s,
                        "  {} {} = {}",
                        c.name,
                        c.sql_type,
                        if from_raw { &c.from_raw } else { &c.from_level }
                    );
                }
            }
            _ => {
                let _ = writeln!(
                    s,
                    "  ?{} {} [{}] {:?}",
                    e.column,
                    e.sql_type,
                    e.aggregates.join(", "),
                    e.mean_representation
                );
            }
        }
    }
    s
}

/// Entries carried forward from the old snapshot that the registry no longer mentions
/// at all are *not* dropped — a disappearance is fatal, so reaching this function means
/// validation already passed and there are none. Kept explicit so the merge is obvious.
pub fn render(entries: &[Allocated], levels: &[AllocatedLevel]) -> String {
    let mut s = String::new();
    s.push_str(
        "# ModernWeather schema snapshot — GENERATED, DO NOT EDIT\n\
         #\n\
         # Written by `cargo xtask migrate` from metrics/registry.toml.\n\
         # This is the record of every identifier that has ever been allocated: the\n\
         # generator diffs the registry against it and refuses identifier reuse,\n\
         # undeprecated disappearance and over-budget schemas. Editing it by hand\n\
         # disables exactly the check it exists to perform.\n\
         #\n\
         # It also carries the column budget: a dropped PostgreSQL column keeps its\n\
         # attribute number forever, so the budget is counted over this file's entries\n\
         # and not over the registry's live ones.\n\n",
    );
    let _ = writeln!(s, "schema_version = 1\n");
    s.push_str(
        "# The aggregate hierarchy as last migrated (N-13). `snapshot::affected_levels`\n\
         # compares each level's rendered definition against this record, so that adding a\n\
         # metric to one level rebuilds that level and not the three whose text did not\n\
         # change. The column list alone would not be enough: inserting the 5-minute level\n\
         # left the hourly level's columns untouched and changed the relation it reads.\n\n",
    );
    for l in levels {
        s.push_str("[[level]]\n");
        let _ = writeln!(s, "name = \"{}\"", l.name);
        let _ = writeln!(s, "bucket = \"{}\"", l.bucket);
        let _ = writeln!(s, "source = \"{}\"", l.source);
        let _ = writeln!(s, "materialized_only = {}", l.materialized_only);
        s.push('\n');
    }
    for e in entries {
        s.push_str("[[allocated]]\n");
        let _ = writeln!(s, "kind = \"{}\"", e.kind);
        let _ = writeln!(s, "key = \"{}\"", e.key);
        let _ = writeln!(s, "column = \"{}\"", e.column);
        let _ = writeln!(s, "sql_type = \"{}\"", e.sql_type);
        let _ = writeln!(s, "field_number = {}", e.field_number);
        let _ = writeln!(s, "proto_type = \"{}\"", e.proto_type);
        let aggs: Vec<String> = e.aggregates.iter().map(|a| format!("\"{a}\"")).collect();
        let _ = writeln!(s, "aggregates = [{}]", aggs.join(", "));
        if let Some(mr) = &e.mean_representation {
            let _ = writeln!(s, "mean_representation = \"{mr}\"");
        }
        let lv: Vec<String> = e.levels.iter().map(|l| format!("\"{l}\"")).collect();
        let _ = writeln!(s, "levels = [{}]", lv.join(", "));
        let _ = writeln!(s, "deprecated = {}", e.deprecated);
        let _ = writeln!(s, "introduced_in = \"{}\"", e.introduced_in);
        s.push('\n');
    }
    s
}

/// One classified difference between the snapshot and the registry.
#[derive(Debug)]
pub struct Change {
    pub class: Class,
    pub key: String,
    pub detail: String,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Class {
    /// A new nullable column. 11.6 ms against ~1 GB of compressed chunks (D-9).
    AddColumn,
    /// Identifiers retained, metric leaves the aggregates (D-21).
    Deprecate,
    /// Aggregates are disposable derived data: drop, regenerate, refresh (D-15).
    RebuildAggregates,
    /// Data-losing or contract-breaking. Refused without --allow-breaking.
    Breaking,
}

impl Class {
    pub fn breaking(self) -> bool {
        self == Class::Breaking
    }
}

pub fn diff(snap: &Snapshot, reg: &Registry) -> Vec<Change> {
    let old = snap.by_key();
    let new = from_registry(reg);
    let mut changes = Vec::new();

    for n in &new {
        let Some(o) = old.get(n.key.as_str()) else {
            // A `metadata` dimension allocates a protobuf field number and nothing
            // else, so saying "new column" of it would be false. The change is still
            // recorded: it is what makes `migrate` write the number into the snapshot,
            // which is what keeps it from ever being handed to anything else (D-21).
            let detail = if n.kind == METADATA_DIMENSION {
                format!(
                    "new metadata dimension {}, protobuf field {} — no column (N-14)",
                    n.key, n.field_number
                )
            } else {
                format!(
                    "new {} {} ({} {}), protobuf field {}",
                    n.kind, n.column, n.column, n.sql_type, n.field_number
                )
            };
            changes.push(Change {
                class: Class::AddColumn,
                key: n.key.clone(),
                detail,
            });
            continue;
        };
        // A dimension changing role across the stored/not-stored line. Neither
        // direction is a no-op and neither was visible before: `column`, `sql_type`,
        // `proto_type` and `deprecated` are all unchanged by a role edit, so without
        // this the diff produced nothing at all and `codegen` quietly rewrote the DDL.
        if o.kind == DIMENSION && n.kind == METADATA_DIMENSION {
            changes.push(Change {
                class: Class::Breaking,
                key: n.key.clone(),
                detail: format!(
                    "role changed to \"metadata\": column {} leaves the generated DDL",
                    o.column
                ),
            });
        }
        if o.kind == METADATA_DIMENSION && n.kind == DIMENSION {
            changes.push(Change {
                class: Class::AddColumn,
                key: n.key.clone(),
                detail: format!(
                    "role changed from \"metadata\": {} ({} {}) becomes a column",
                    n.key, n.column, n.sql_type
                ),
            });
        }
        if o.column != n.column {
            changes.push(Change {
                class: Class::Breaking,
                key: n.key.clone(),
                detail: format!("column renamed {} -> {}", o.column, n.column),
            });
        }
        if o.sql_type != n.sql_type {
            changes.push(Change {
                class: Class::Breaking,
                key: n.key.clone(),
                detail: format!("SQL type changed {} -> {}", o.sql_type, n.sql_type),
            });
        }
        if o.proto_type != n.proto_type {
            changes.push(Change {
                class: Class::Breaking,
                key: n.key.clone(),
                detail: format!("protobuf type changed {} -> {}", o.proto_type, n.proto_type),
            });
        }
        if o.aggregates != n.aggregates {
            changes.push(Change {
                class: Class::Breaking,
                key: n.key.clone(),
                detail: format!(
                    "aggregation strategy changed [{}] -> [{}]",
                    o.aggregates.join(", "),
                    n.aggregates.join(", ")
                ),
            });
        }
        if o.mean_representation != n.mean_representation {
            changes.push(Change {
                class: Class::Breaking,
                key: n.key.clone(),
                detail: format!(
                    "mean representation changed {:?} -> {:?}",
                    o.mean_representation, n.mean_representation
                ),
            });
        }
        if !o.deprecated && n.deprecated {
            let detail = if n.kind == METADATA_DIMENSION {
                format!(
                    "metadata dimension {} retired; protobuf field {} stays reserved",
                    n.key, n.field_number
                )
            } else {
                format!("{} deprecated; identifiers retained", n.column)
            };
            changes.push(Change {
                class: Class::Deprecate,
                key: n.key.clone(),
                detail,
            });
        }
        if o.deprecated && !n.deprecated {
            changes.push(Change {
                class: Class::Breaking,
                key: n.key.clone(),
                detail: "undeprecated — an identifier that was retired is being revived".into(),
            });
        }
        if o.levels != n.levels {
            changes.push(Change {
                class: Class::RebuildAggregates,
                key: n.key.clone(),
                detail: format!(
                    "level membership changed [{}] -> [{}]",
                    o.levels.join(", "),
                    n.levels.join(", ")
                ),
            });
        }
    }
    changes
}

/// Which aggregate levels a registry change forces a rebuild of (N-13).
///
/// Aggregates are disposable derived data and the supported change is drop, regenerate,
/// refresh (D-15), so the only question is *which* levels. Until this compared rendered
/// definitions it keyed off which metrics changed, and inserting the 5-minute level
/// changed every metric's `levels` list — so all four levels were dropped, including the
/// minute level, whose definition was byte-identical before and after. An intact minute
/// level would have built the new 5-minute level from it in 59 s instead of forcing a
/// 401 s full raw rescan (OQ-W4-1, docs/database-schema.md §6 measurement 3).
///
/// A level is rebuilt when its definition is not the one the snapshot records, and
/// otherwise it is left alone. A level the snapshot does not mention is new, and new is
/// a difference.
pub fn affected_levels(reg: &Registry, prev: &Snapshot) -> Vec<String> {
    // A snapshot written before M1-6 records no levels, and an empty one records nothing at
    // all. Neither needs its own branch: `find` below returns `None` for every level, so
    // every level is a difference and every level is rebuilt. That is over-rebuilding,
    // which is the behaviour this function had for its whole life and the safe direction.
    let new_entries = from_registry(reg);
    // The two dimension columns, resolved on each side separately — the whole point is to
    // notice when they differ. Matched by stable key rather than by name: a key is an
    // identifier and reusing one is already fatal, so it is the only thing that reliably
    // names the same dimension on both sides of a rename. A dimension the snapshot has
    // never seen falls back to its current column, which makes it compare equal; it is new
    // on this run and a new dimension does not change an existing level.
    let column_of = |entries: &[Allocated], key: &str, fallback: &str| -> String {
        entries
            .iter()
            .find(|e| e.key == key)
            .map_or_else(|| fallback.to_string(), |e| e.column.clone())
    };
    let (t_key, t_col) = {
        let d = reg.time_column();
        (d.key.as_str(), d.column.as_str())
    };
    let (p_key, p_col) = {
        let d = reg.partition_column();
        (d.key.as_str(), d.column.as_str())
    };
    let t_new = column_of(&new_entries, t_key, t_col);
    let p_new = column_of(&new_entries, p_key, p_col);
    let t_old = column_of(&prev.entries, t_key, t_col);
    let p_old = column_of(&prev.entries, p_key, p_col);

    let mut out = Vec::new();
    for l in &reg.levels {
        let after = rendered_level(&AllocatedLevel::from_level(l), &new_entries, &t_new, &p_new);
        let before = prev
            .levels
            .iter()
            .find(|q| q.name == l.name)
            .map(|q| rendered_level(q, &prev.entries, &t_old, &p_old));
        if before.as_deref() != Some(after.as_str()) {
            out.push(l.name.clone());
        }
    }
    out
}

/// Levels the snapshot records and the registry no longer declares.
///
/// They are not in `affected_levels` because they are not rebuilt — they are dropped and
/// not recreated, which is a different statement and needs a different list. Before the
/// snapshot recorded levels there was no way to produce this one at all, and the
/// consequence was that removing a `[[level]]` left its continuous aggregate, its refresh
/// policy and its retention policy in every deployed database forever, reading the level
/// below it, with no file under `generated/` describing it. `codegen` prunes the generated
/// file (N-15) and nothing dropped the relation.
pub fn removed_levels(reg: &Registry, prev: &Snapshot) -> Vec<String> {
    prev.levels
        .iter()
        .filter(|p| reg.level(&p.name).is_none())
        .map(|p| p.name.clone())
        .collect()
}

pub fn strategy_names(s: &[Strategy]) -> Vec<String> {
    s.iter().map(|x| x.name().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry with an arbitrary level chain and one metric per named level set.
    /// `levels` is the chain as `(name, bucket, source)`; `carried` is the metric's own
    /// level list.
    fn reg(levels: &[(&str, &str, &str)], carried: &[&str], unit: &str) -> Registry {
        let mut t =
            String::from("schema_version = 1\n[defaults]\nmean_representation = \"sum_count\"\n");
        let names: Vec<String> = levels.iter().map(|l| format!("\"{}\"", l.0)).collect();
        t.push_str(&format!("levels = [{}]\n", names.join(", ")));
        for (name, bucket, source) in levels {
            t.push_str(&format!(
                "[[level]]\nname = \"{name}\"\nbucket = \"{bucket}\"\nsource = \"{source}\"\n"
            ));
        }
        t.push_str(
            "[[dimension]]\nkey = \"time\"\ncolumn = \"time\"\nsql_type = \"timestamptz\"\n\
             role = \"time\"\nfield_number = 1\nproto_name = \"t\"\n\
             proto_type = \"google.protobuf.Timestamp\"\nlabel = \"Time\"\n\
             introduced_in = \"0.1.0\"\n\
             [[dimension]]\nkey = \"station_id\"\ncolumn = \"station_id\"\nsql_type = \"text\"\n\
             role = \"partition\"\nfield_number = 221\nproto_name = \"s\"\n\
             proto_type = \"string\"\nlabel = \"Station\"\nintroduced_in = \"0.1.0\"\n\
             [[unit]]\nname = \"degF\"\nlabel = \"F\"\n",
        );
        let carried: Vec<String> = carried.iter().map(|l| format!("\"{l}\"")).collect();
        let aggs = if carried.is_empty() { "" } else { "\"mean\"" };
        t.push_str(&format!(
            "[[metric]]\nkey = \"a\"\ncolumn = \"ca\"\nsql_type = \"float4\"\n\
             field_number = 4\nproto_name = \"a\"\nproto_type = \"float\"\nlabel = \"A\"\n\
             unit = \"{unit}\"\ntier = \"core\"\naggregates = [{aggs}]\n\
             levels = [{}]\nintroduced_in = \"0.1.0\"\n",
            carried.join(", ")
        ));
        toml::from_str(&t).expect("fixture parses")
    }

    /// The snapshot `migrate` would write for this registry, through the real renderer
    /// and the real parser — so a level record that does not round-trip fails here.
    fn snap(r: &Registry) -> Snapshot {
        let text = render(&from_registry(r), &levels_from_registry(r));
        toml::from_str(&text).expect("the rendered snapshot parses")
    }

    const THREE: &[(&str, &str, &str)] = &[
        ("1m", "1 minute", "raw"),
        ("5m", "5 minutes", "1m"),
        ("1h", "1 hour", "5m"),
    ];

    /// **The case N-13 exists for.** Inserting the 5-minute level changes every metric's
    /// `levels` list, and the old rule keyed off exactly that — so all three levels were
    /// dropped and recreated, including the minute level, whose definition is
    /// byte-identical before and after. Rebuilding the minute level is what turns a 59 s
    /// build of the new level from the intact one into a full raw rescan (OQ-W4-1).
    #[test]
    fn inserting_a_level_leaves_the_level_below_it_alone() {
        let before = reg(
            &[("1m", "1 minute", "raw"), ("1h", "1 hour", "1m")],
            &["1m", "1h"],
            "degF",
        );
        let s = snap(&before);
        let after = reg(THREE, &["1m", "5m", "1h"], "degF");

        let levels = affected_levels(&after, &s);
        assert!(
            !levels.contains(&"1m".to_string()),
            "the minute level's definition did not change: {levels:?}"
        );
        assert_eq!(levels, vec!["5m".to_string(), "1h".to_string()]);
    }

    /// The hourly level is in that list for a reason the column comparison alone would
    /// have missed: its columns are unchanged and it now reads `observations_5m` instead
    /// of `observations_1m`. That is why the snapshot records each level's own definition
    /// and not just its membership.
    #[test]
    fn a_level_that_only_changed_the_relation_it_reads_is_still_rebuilt() {
        let before = reg(
            &[("1m", "1 minute", "raw"), ("1h", "1 hour", "1m")],
            &["1m", "1h"],
            "degF",
        );
        let s = snap(&before);
        let after = reg(THREE, &["1m", "5m", "1h"], "degF");

        let old_1h = s.levels.iter().find(|l| l.name == "1h").expect("recorded");
        let new_1h = AllocatedLevel::from_level(after.level("1h").expect("declared"));
        let (t, p) = ("time", "station_id");
        assert_eq!(
            rendered_level(old_1h, &s.entries, t, p)
                .lines()
                .skip(1)
                .collect::<Vec<_>>(),
            rendered_level(&new_1h, &from_registry(&after), t, p)
                .lines()
                .skip(1)
                .collect::<Vec<_>>(),
            "its column list is identical"
        );
        assert!(affected_levels(&after, &s).contains(&"1h".to_string()));
    }

    /// A change the aggregates do not see costs no rebuild at all. Before this, every
    /// `AddColumn` was "touched", and the old rule's `in_level` guard was the only thing
    /// keeping a raw-only metric from dropping all three levels.
    #[test]
    fn a_registry_change_the_levels_cannot_see_rebuilds_nothing() {
        let before = reg(THREE, &["1m", "5m", "1h"], "degF");
        let s = snap(&before);
        // Same levels, same metric, same aggregates: only the unit string moved.
        let after = reg(THREE, &["1m", "5m", "1h"], "degF");
        assert!(affected_levels(&after, &s).is_empty());
    }

    /// A metric leaving the aggregates changes the text of every level that carried it,
    /// so the rebuild still happens. The new rule must not be quieter than the old one
    /// where the old one was right.
    #[test]
    fn deprecating_a_carried_metric_rebuilds_every_level_that_carried_it() {
        let before = reg(THREE, &["1m", "5m", "1h"], "degF");
        let s = snap(&before);
        let mut after = reg(THREE, &["1m", "5m", "1h"], "degF");
        after.metrics[0].deprecated = true;
        assert_eq!(affected_levels(&after, &s), vec!["1m", "5m", "1h"]);
    }

    /// Changing a bucket width changes the view and nothing else, and the column
    /// comparison cannot see it. This is the whole reason the level's own definition is
    /// recorded alongside the entries.
    #[test]
    fn widening_a_bucket_rebuilds_that_level() {
        let before = reg(THREE, &["1m", "5m", "1h"], "degF");
        let s = snap(&before);
        let after = reg(
            &[
                ("1m", "1 minute", "raw"),
                ("5m", "10 minutes", "1m"),
                ("1h", "1 hour", "5m"),
            ],
            &["1m", "5m", "1h"],
            "degF",
        );
        assert_eq!(affected_levels(&after, &s), vec!["5m"]);
    }

    /// A snapshot written before M1-6 has no `[[level]]` section, so there is no previous
    /// definition to compare against and the answer has to be "all of them". Rebuilding
    /// more than necessary is what this function did for its whole life; rebuilding fewer
    /// would leave a level materialised from a definition it no longer has.
    #[test]
    fn a_snapshot_with_no_level_record_rebuilds_every_level() {
        let after = reg(THREE, &["1m", "5m", "1h"], "degF");
        let mut s = snap(&after);
        s.levels.clear();
        assert_eq!(affected_levels(&after, &s), vec!["1m", "5m", "1h"]);
    }

    /// And an empty snapshot is a bootstrap, not a pre-M1-6 file: every level is new.
    #[test]
    fn an_empty_snapshot_rebuilds_every_level() {
        let after = reg(THREE, &["1m", "5m", "1h"], "degF");
        let empty = Snapshot {
            schema_version: 1,
            levels: vec![],
            entries: vec![],
        };
        assert_eq!(affected_levels(&after, &empty), vec!["1m", "5m", "1h"]);
    }

    /// Renaming the partition column rewrites every generated level — it is in the SELECT
    /// and in the GROUP BY of all four — and before the dimension columns were part of the
    /// rendered definition this returned nothing to rebuild. PostgreSQL propagates
    /// RENAME COLUMN into a dependent view's body but not into its output column names, so
    /// a migrated database got a view whose body and output disagreed while a fresh install
    /// did not: D-8's drift, from the tool that exists to prevent it.
    #[test]
    fn renaming_the_partition_column_rebuilds_every_level() {
        let before = reg(THREE, &["1m", "5m", "1h"], "degF");
        let s = snap(&before);
        let mut after = reg(THREE, &["1m", "5m", "1h"], "degF");
        let p = after
            .dimensions
            .iter_mut()
            .find(|d| d.key == "station_id")
            .expect("declared");
        p.column = "station".into();
        assert_eq!(affected_levels(&after, &s), vec!["1m", "5m", "1h"]);
    }

    /// The same for the time column, which is the first level's `time_bucket` argument.
    #[test]
    fn renaming_the_time_column_rebuilds_every_level() {
        let before = reg(THREE, &["1m", "5m", "1h"], "degF");
        let s = snap(&before);
        let mut after = reg(THREE, &["1m", "5m", "1h"], "degF");
        let t = after
            .dimensions
            .iter_mut()
            .find(|d| d.key == "time")
            .expect("declared");
        t.column = "observed_at".into();
        assert_eq!(affected_levels(&after, &s), vec!["1m", "5m", "1h"]);
    }

    /// A level removed from the registry is dropped, not rebuilt, so it is a separate
    /// list. Before the snapshot recorded levels this list could not be computed at all,
    /// and removing a `[[level]]` left its continuous aggregate and both its policies in
    /// every deployed database forever, reading the level below it.
    #[test]
    fn a_level_removed_from_the_registry_is_reported_as_gone_and_not_as_affected() {
        let before = reg(THREE, &["1m", "5m", "1h"], "degF");
        let s = snap(&before);
        let after = reg(
            &[("1m", "1 minute", "raw"), ("5m", "5 minutes", "1m")],
            &["1m", "5m"],
            "degF",
        );
        assert_eq!(removed_levels(&after, &s), vec!["1h"]);
        assert!(
            !affected_levels(&after, &s).contains(&"1h".to_string()),
            "a level that is gone is not rebuilt"
        );
    }

    /// The migration has to carry the drop, and must not also recreate it. `migrate_cmd`
    /// reaches `render` only when something is pending, which is the other half of this —
    /// see `a_level_only_change_is_not_nothing_to_emit`.
    #[test]
    fn a_removed_level_is_dropped_by_the_migration_and_not_recreated() {
        let before = reg(THREE, &["1m", "5m", "1h"], "degF");
        let s = snap(&before);
        let after = reg(
            &[("1m", "1 minute", "raw"), ("5m", "5 minutes", "1m")],
            &["1m", "5m"],
            "degF",
        );
        let changes = diff(&s, &after);
        let m = crate::migrate::render(&after, &s, &changes, false, "20260101000000", "drop_1h");
        assert!(
            m.body
                .contains("DROP MATERIALIZED VIEW IF EXISTS observations_1h CASCADE;"),
            "the level has to actually be dropped:\n{}",
            m.body
        );
        assert!(
            !m.body.contains("CREATE MATERIALIZED VIEW observations_1h"),
            "and must not come back:\n{}",
            m.body
        );
        assert!(m.body.contains("dropped and NOT recreated"), "{}", m.body);
    }

    /// `changes.is_empty()` used to be the whole test for "the snapshot agrees with the
    /// registry", and since the snapshot records levels it is not: a bucket change produces
    /// no entry-level `Change` at all. `migrate_cmd` returned on the change count and
    /// printed "registry and snapshot agree", while `codegen` rewrote the level's SQL.
    /// This is the comparison that makes the message true.
    #[test]
    fn a_level_only_change_is_not_nothing_to_emit() {
        let before = reg(THREE, &["1m", "5m", "1h"], "degF");
        let s = snap(&before);
        let after = reg(
            &[
                ("1m", "1 minute", "raw"),
                ("5m", "10 minutes", "1m"),
                ("1h", "1 hour", "5m"),
            ],
            &["1m", "5m", "1h"],
            "degF",
        );
        assert!(
            diff(&s, &after).is_empty(),
            "no entry changed, which is exactly why the level record is needed"
        );
        assert_ne!(
            levels_from_registry(&after),
            s.levels,
            "and the level comparison is what notices"
        );
        assert_eq!(affected_levels(&after, &s), vec!["5m"]);
    }

    #[test]
    fn nothing_is_reported_gone_when_the_hierarchy_is_unchanged() {
        let r = reg(THREE, &["1m", "5m", "1h"], "degF");
        assert!(removed_levels(&r, &snap(&r)).is_empty());
    }

    /// The shipped registry against the shipped snapshot: nothing pending, so nothing to
    /// rebuild. A migration generated today that dropped a level would be dropping it for
    /// no reason.
    #[test]
    fn the_shipped_registry_forces_no_rebuild() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let r = Registry::load(&root.join("metrics/registry.toml")).expect("loads");
        let s = Snapshot::load(&root.join("metrics/snapshot.toml")).expect("loads");
        assert!(
            !s.levels.is_empty(),
            "the committed snapshot records the level hierarchy (N-13)"
        );
        assert!(affected_levels(&r, &s).is_empty());
        assert!(removed_levels(&r, &s).is_empty());
    }
}
