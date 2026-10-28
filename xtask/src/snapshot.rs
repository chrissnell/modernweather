//! The committed schema snapshot: the record of every identifier that has ever been
//! allocated, and the only thing that makes "append-only" enforceable.
//!
//! The registry alone cannot enforce it — a human editing TOML can rename anything.
//! The generator diffs the registry against this file, which is written only by
//! `cargo xtask migrate`, and refuses the changes §2 of docs/metric-registry.md calls
//! fatal.

use crate::registry::{MeanRepr, Registry, Strategy};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub schema_version: u32,
    #[serde(default, rename = "allocated")]
    pub entries: Vec<Allocated>,
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
            kind: "dimension".into(),
            key: d.key.clone(),
            column: d.column.clone(),
            sql_type: d.sql_type.ddl().into(),
            field_number: d.field_number,
            proto_type: d.proto_type.clone(),
            aggregates: vec![],
            mean_representation: None,
            levels: vec![],
            deprecated: false,
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
            mean_representation: Some(
                match m.mean_repr(&reg.defaults) {
                    MeanRepr::SumCount => "sum_count",
                    MeanRepr::StatsAgg => "stats_agg",
                }
                .to_string(),
            ),
            levels: m.levels(&reg.defaults).to_vec(),
            deprecated: m.deprecated,
            introduced_in: m.introduced_in.clone(),
        });
    }
    out
}

/// Entries carried forward from the old snapshot that the registry no longer mentions
/// at all are *not* dropped — a disappearance is fatal, so reaching this function means
/// validation already passed and there are none. Kept explicit so the merge is obvious.
pub fn render(entries: &[Allocated]) -> String {
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
            changes.push(Change {
                class: Class::AddColumn,
                key: n.key.clone(),
                detail: format!(
                    "new {} {} ({} {}), protobuf field {}",
                    n.kind, n.column, n.column, n.sql_type, n.field_number
                ),
            });
            continue;
        };
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
            changes.push(Change {
                class: Class::Deprecate,
                key: n.key.clone(),
                detail: format!("{} deprecated; identifiers retained", n.column),
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

/// Which aggregate levels a change set forces a rebuild of.
pub fn affected_levels(reg: &Registry, changes: &[Change]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let touched: Vec<&str> = changes
        .iter()
        .filter(|c| {
            matches!(
                c.class,
                Class::RebuildAggregates | Class::Deprecate | Class::AddColumn | Class::Breaking
            )
        })
        .map(|c| c.key.as_str())
        .collect();
    for lvl in &reg.levels {
        let hit = reg.metrics.iter().any(|m| {
            touched.contains(&m.key.as_str())
                && (m.in_level(&reg.defaults, &lvl.name)
                    || m.levels(&reg.defaults).iter().any(|l| l == &lvl.name))
        });
        if hit {
            out.push(lvl.name.clone());
        }
    }
    out
}

pub fn strategy_names(s: &[Strategy]) -> Vec<String> {
    s.iter().map(|x| x.name().to_string()).collect()
}
