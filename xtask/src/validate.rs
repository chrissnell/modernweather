//! Invariant enforcement. §2 of docs/metric-registry.md is this file.
//!
//! Two tiers, and the distinction is the whole design: a **fatal** error has no flag
//! because there is no correct way to proceed — the identifier is already in a
//! deployed protobuf or a PostgreSQL attribute number. A **breaking** change is
//! refused by default but is a real operation an operator may need, so it has one
//! loud flag and a migration that says what it will destroy.

use crate::registry::{Registry, Role, SqlType, Strategy};
use crate::snapshot::{self, Class, Snapshot};
use std::collections::BTreeMap;

/// PostgreSQL's hard limit, confirmed by probe on PostgreSQL 17 (D-19).
pub const COLUMN_LIMIT: usize = 1600;
/// A compressed hypertable's shadow table mirrors every column, dropped ones
/// included, and adds five of its own: `_ts_meta_count`, `_ts_meta_min_1`,
/// `_ts_meta_max_1`, `_ts_meta_v2_first_time`, `_ts_meta_v2_last_time`.
pub const COMPRESSION_METADATA_COLUMNS: usize = 5;
/// The effective ceiling for the raw hypertable: 1600 - 5.
pub const RAW_COLUMN_CEILING: usize = COLUMN_LIMIT - COMPRESSION_METADATA_COLUMNS;
/// A continuous aggregate may emit at most this many aggregate expressions, because
/// its two grouping columns also occupy slots: 1598 + 2 = 1600 created, 1599 + 2
/// refused with `tables can have at most 1600 columns`.
pub const AGG_EXPRESSION_CEILING: usize = COLUMN_LIMIT - 2;

pub struct Budget {
    pub allocated_columns: usize,
    pub live_columns: usize,
    pub per_level: Vec<(String, usize, usize)>, // (level, metrics, expressions)
}

pub fn fatal(msgs: Vec<String>) -> Result<(), String> {
    if msgs.is_empty() {
        Ok(())
    } else {
        Err(msgs.join("\n"))
    }
}

/// Checks that need only the registry: uniqueness within the file, structural
/// coherence, and strategies that are legal for the column's type.
pub fn registry_internal(reg: &Registry) -> Result<(), String> {
    let mut errs = Vec::new();

    if reg.schema_version != 1 {
        errs.push(format!(
            "FATAL: registry schema_version {} is not supported (expected 1)",
            reg.schema_version
        ));
    }

    // Keys, columns and protobuf field numbers are one namespace shared by
    // dimensions and metrics: they land in the same table and the same message.
    let mut keys: BTreeMap<&str, &str> = BTreeMap::new();
    let mut cols: BTreeMap<&str, &str> = BTreeMap::new();
    let mut nums: BTreeMap<u32, &str> = BTreeMap::new();
    let note = |m: &mut Vec<String>, what: &str, val: String, first: &str, second: &str| {
        m.push(format!(
            "FATAL: {what} {val} is allocated to two registry entries: '{first}' and '{second}'.\n\
             \x20      Identifiers are allocated once and never reused (D-21). Allocate a new one \
             for '{second}'."
        ));
    };

    let all: Vec<(&str, &str, u32, &str)> = reg
        .dimensions
        .iter()
        .map(|d| {
            (
                d.key.as_str(),
                d.column.as_str(),
                d.field_number,
                "dimension",
            )
        })
        .chain(
            reg.metrics
                .iter()
                .map(|m| (m.key.as_str(), m.column.as_str(), m.field_number, "metric")),
        )
        .collect();

    for (key, col, num, _kind) in &all {
        if let Some(prev) = keys.insert(key, key) {
            note(&mut errs, "stable key", format!("'{key}'"), prev, key);
        }
        if let Some(prev) = cols.insert(col, key) {
            note(&mut errs, "column name", format!("'{col}'"), prev, key);
        }
        if let Some(prev) = nums.insert(*num, key) {
            note(
                &mut errs,
                "protobuf field number",
                num.to_string(),
                prev,
                key,
            );
        }
        if *num == 0 || (19000..=19999).contains(num) {
            errs.push(format!(
                "FATAL: protobuf field number {num} on '{key}' is not usable \
                 (0 and 19000-19999 are reserved)"
            ));
        }
    }

    // Exactly one time dimension and one partition dimension.
    for (role, label) in [(Role::Time, "time"), (Role::Partition, "partition")] {
        let n = reg.dimensions.iter().filter(|d| d.role == role).count();
        if n != 1 {
            errs.push(format!(
                "FATAL: expected exactly one dimension with role = \"{label}\", found {n}"
            ));
        }
    }

    // The level chain: first level reads raw, each later level reads the one before it.
    if reg.levels.is_empty() {
        errs.push("FATAL: no [[level]] declared".into());
    }
    for (i, l) in reg.levels.iter().enumerate() {
        if i == 0 {
            if l.source != "raw" {
                errs.push(format!(
                    "FATAL: the first level '{}' must read from \"raw\", not \"{}\"",
                    l.name, l.source
                ));
            }
        } else {
            let expect = &reg.levels[i - 1].name;
            if &l.source != expect {
                errs.push(format!(
                    "FATAL: level '{}' reads \"{}\" but must read the level below it, '{}' \
                     (D-11: no level reads raw except the first)",
                    l.name, l.source, expect
                ));
            }
        }
    }

    let level_names: Vec<&str> = reg.levels.iter().map(|l| l.name.as_str()).collect();
    for l in &reg.defaults.levels {
        if !level_names.contains(&l.as_str()) {
            errs.push(format!(
                "FATAL: [defaults].levels names '{l}', which is not a declared [[level]]"
            ));
        }
    }

    // Level membership must be contiguous from the first level. A level reads the
    // level below it (D-11), so a metric at "1h" that "1m" does not carry has no
    // source column to roll up — the generator would emit SQL referencing a column
    // that does not exist. This is the mechanical reason OQ-5 cannot be answered as
    // written; see docs/metric-registry.md OQ-W2-1.
    for m in &reg.metrics {
        let mine = m.levels(&reg.defaults);
        let mut gap: Option<(&str, &str)> = None;
        for (i, l) in level_names.iter().enumerate() {
            let has = mine.iter().any(|x| x == l);
            if !has {
                // every later level must also be absent
                for later in &level_names[i + 1..] {
                    if mine.iter().any(|x| x == later) {
                        gap = Some((l, later));
                    }
                }
            }
        }
        if let Some((missing, present)) = gap {
            errs.push(format!(
                "FATAL: metric '{}' is carried at level '{present}' but not at '{missing}'.\n\
                 \x20      Level membership must be contiguous from '{}': '{present}' reads \
                 '{missing}' (D-11), so there would be no source column to roll up. Either add \
                 '{missing}' to its levels or drop '{present}'.",
                m.key, level_names[0]
            ));
        }
    }

    for m in &reg.metrics {
        for l in m.levels(&reg.defaults) {
            if !level_names.contains(&l.as_str()) {
                errs.push(format!(
                    "FATAL: metric '{}' names level '{l}', which is not a declared [[level]]",
                    m.key
                ));
            }
        }
        if !m.sql_type.numeric() && !m.aggregates.is_empty() {
            errs.push(format!(
                "FATAL: metric '{}' is {} and has aggregates [{}]. Text, timestamp and \
                 enumerated columns are raw-only: set aggregates = [].",
                m.key,
                m.sql_type,
                snapshot::strategy_names(&m.aggregates).join(", ")
            ));
        }
        if m.aggregates.contains(&Strategy::CircularMean) && m.sql_type != SqlType::Float4 {
            errs.push(format!(
                "FATAL: metric '{}' uses circular_mean on a {} column; an angle must be \
                 float4",
                m.key, m.sql_type
            ));
        }
        if m.aggregates.contains(&Strategy::Mean) && m.aggregates.contains(&Strategy::Total) {
            errs.push(format!(
                "FATAL: metric '{}' asks for both mean and total. Both emit a sum; pick one.",
                m.key
            ));
        }
        let mut seen = Vec::new();
        for s in &m.aggregates {
            if seen.contains(s) {
                errs.push(format!(
                    "FATAL: metric '{}' lists aggregate '{}' twice",
                    m.key, s
                ));
            }
            seen.push(*s);
        }
        if m.deprecated && m.deprecated_in.is_none() {
            errs.push(format!(
                "FATAL: metric '{}' is deprecated but has no deprecated_in version",
                m.key
            ));
        }
        if m.levels(&reg.defaults).is_empty() != m.aggregates.is_empty() {
            errs.push(format!(
                "FATAL: metric '{}' has aggregates [{}] and levels [{}]. A metric with \
                 aggregates must name at least one level, and a raw-only metric must name \
                 none.",
                m.key,
                snapshot::strategy_names(&m.aggregates).join(", "),
                m.levels(&reg.defaults).join(", ")
            ));
        }
        for agg in m.legacy_aggregate_rest_keys.keys() {
            if !m.aggregates.iter().any(|s| s.name() == agg) {
                errs.push(format!(
                    "FATAL: metric '{}' maps a legacy key onto aggregate '{agg}', which it \
                     does not compute",
                    m.key
                ));
            }
        }
    }

    // A legacy REST key may be attached to more than one metric only if someone
    // deliberately records a duplicated legacy key. Warn loudly rather than fail:
    // W1 owns the surface and the duplication is already shipped.
    let mut rest: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for m in &reg.metrics {
        for k in &m.legacy_rest_keys {
            rest.entry(k).or_default().push(&m.key);
        }
    }
    for (k, owners) in rest {
        if owners.len() > 1 {
            errs.push(format!(
                "FATAL: legacy REST key '{k}' is claimed by {} metrics ({}). One JSON key \
                 cannot be generated from two columns.",
                owners.len(),
                owners.join(", ")
            ));
        }
    }

    fatal(errs)
}

/// Checks that need the snapshot: identifier reuse across entries, disappearance
/// without deprecation, and the column budget.
pub fn against_snapshot(
    reg: &Registry,
    snap: &Snapshot,
    allow_breaking: bool,
) -> Result<Vec<snapshot::Change>, String> {
    let mut errs = Vec::new();

    if snap.schema_version != 1 {
        errs.push(format!(
            "FATAL: snapshot schema_version {} is not supported (expected 1)",
            snap.schema_version
        ));
    }

    let new = snapshot::from_registry(reg);
    let new_keys: BTreeMap<&str, &snapshot::Allocated> =
        new.iter().map(|e| (e.key.as_str(), e)).collect();

    // An identifier that was allocated to one entry turning up on another.
    let mut col_owner: BTreeMap<&str, &str> = BTreeMap::new();
    let mut num_owner: BTreeMap<u32, &str> = BTreeMap::new();
    for e in &snap.entries {
        col_owner.insert(&e.column, &e.key);
        num_owner.insert(e.field_number, &e.key);
    }
    for e in &new {
        if let Some(owner) = col_owner.get(e.column.as_str()) {
            if *owner != e.key.as_str() {
                errs.push(format!(
                    "FATAL: column '{}' was allocated to '{owner}' and now appears on '{}'.\n\
                     \x20      A PostgreSQL attribute number is never released, so this column \
                     cannot be reallocated. Choose a new column name.",
                    e.column, e.key
                ));
            }
        }
        if let Some(owner) = num_owner.get(&e.field_number) {
            if *owner != e.key.as_str() {
                errs.push(format!(
                    "FATAL: protobuf field number {} was allocated to '{owner}' and now \
                     appears on '{}'.\n\
                     \x20      Reusing a field number silently reinterprets historical data on \
                     every client that has not been rebuilt (D-21). Allocate the next free \
                     number instead.",
                    e.field_number, e.key
                ));
            }
        }
    }

    // Disappearance without deprecation.
    for o in &snap.entries {
        if !new_keys.contains_key(o.key.as_str()) {
            errs.push(format!(
                "FATAL: '{}' (column '{}', protobuf field {}) is in the snapshot and gone \
                 from the registry.\n\
                 \x20      Removing a metric does not delete its entry: set deprecated = true \
                 and deprecated_in = \"<version>\" so the identifiers stay allocated (D-21).",
                o.key, o.column, o.field_number
            ));
        }
    }

    // Column budget, counted over allocated identifiers rather than live ones (H-4).
    let allocated = union_allocated(snap, &new);
    if allocated > RAW_COLUMN_CEILING {
        errs.push(format!(
            "FATAL: the raw hypertable would occupy {allocated} allocated columns, over the \
             effective ceiling of {RAW_COLUMN_CEILING}.\n\
             \x20      PostgreSQL's hard limit is {COLUMN_LIMIT} columns, and a compressed \
             hypertable's shadow table mirrors every column — dropped ones included — and adds \
             {COMPRESSION_METADATA_COLUMNS} of its own. Dropped columns are charged twice and \
             are reclaimed only by a full table rewrite."
        ));
    }

    for lvl in &reg.levels {
        let n = reg.level_columns(&lvl.name).len();
        if n > AGG_EXPRESSION_CEILING {
            errs.push(format!(
                "FATAL: aggregate '{}' would emit {n} aggregate expressions, over the ceiling \
                 of {AGG_EXPRESSION_CEILING}.\n\
                 \x20      A continuous aggregate's two grouping columns also occupy slots, so \
                 {AGG_EXPRESSION_CEILING} + 2 = {COLUMN_LIMIT} is the widest view PostgreSQL \
                 will create. Reduce the metrics carried at this level or their aggregation \
                 strategies.",
                lvl.name
            ));
        }
    }

    fatal(errs)?;

    let changes = snapshot::diff(snap, reg);
    let breaking: Vec<&snapshot::Change> = changes.iter().filter(|c| c.class.breaking()).collect();
    if !breaking.is_empty() && !allow_breaking {
        let mut m = String::from(
            "REFUSED: the registry asks for changes that destroy data or break a shipped \
             contract.\n",
        );
        for c in &breaking {
            m.push_str(&format!("  - {}: {}\n", c.key, c.detail));
        }
        m.push_str(
            "\nEach of these is a real operation, not a mistake, so there is one flag:\n\
             \n\
             \x20   cargo xtask migrate --allow-breaking\n\
             \n\
             It will emit the migration with the destructive statements written out and a\n\
             header naming every one of them. Read the migration before you run it: a column\n\
             rename abandons the old column's data where it is, a SQL type change rewrites\n\
             every chunk, and a protobuf type change breaks every client that has not been\n\
             rebuilt. `codegen` has no such flag — generated output must always match the\n\
             snapshot.",
        );
        return Err(m);
    }
    Ok(changes)
}

/// Identifiers in the snapshot, plus any the registry adds. The snapshot may also
/// hold identifiers the registry no longer lists — which validation rejects, but the
/// count has to include them for the budget to mean what H-4 says it means.
fn union_allocated(snap: &Snapshot, new: &[snapshot::Allocated]) -> usize {
    let mut keys: Vec<&str> = snap.entries.iter().map(|e| e.key.as_str()).collect();
    for e in new {
        if !keys.contains(&e.key.as_str()) {
            keys.push(&e.key);
        }
    }
    keys.len()
}

pub fn budget(reg: &Registry, snap: Option<&Snapshot>) -> Budget {
    let new = snapshot::from_registry(reg);
    let allocated = match snap {
        Some(s) => union_allocated(s, &new),
        None => new.len(),
    };
    Budget {
        allocated_columns: allocated,
        live_columns: reg.dimensions.len() + reg.live_metrics().len(),
        per_level: reg
            .levels
            .iter()
            .map(|l| {
                let cols = reg.level_columns(&l.name);
                let metrics = reg
                    .metrics
                    .iter()
                    .filter(|m| m.in_level(&reg.defaults, &l.name))
                    .count();
                (l.name.clone(), metrics, cols.len())
            })
            .collect(),
    }
}

pub fn class_label(c: Class) -> &'static str {
    match c {
        Class::AddColumn => "add nullable column",
        Class::Deprecate => "deprecate",
        Class::RebuildAggregates => "rebuild aggregates",
        Class::Breaking => "BREAKING",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;
    use crate::snapshot::Snapshot;

    const PRELUDE: &str = r#"
schema_version = 1
[defaults]
mean_representation = "sum_count"
levels = ["1m", "1h"]
[[level]]
name = "1m"
bucket = "1 minute"
source = "raw"
[[level]]
name = "1h"
bucket = "1 hour"
source = "1m"
[[dimension]]
key = "time"
column = "time"
sql_type = "timestamptz"
role = "time"
field_number = 1
proto_name = "readingTimestamp"
proto_type = "google.protobuf.Timestamp"
label = "Time"
introduced_in = "0.1.0"
[[dimension]]
key = "station_id"
column = "station_id"
sql_type = "text"
role = "partition"
field_number = 221
proto_name = "station_id"
proto_type = "string"
label = "Station"
introduced_in = "0.1.0"
"#;

    fn metric(key: &str, column: &str, num: u32, extra: &str) -> String {
        format!(
            "[[metric]]\nkey = \"{key}\"\ncolumn = \"{column}\"\nsql_type = \"float4\"\n\
             field_number = {num}\nproto_name = \"{key}\"\nproto_type = \"float\"\n\
             label = \"L\"\nunit = \"degF\"\ntier = \"core\"\n\
             aggregates = [\"mean\"]\nlevels = [\"1m\", \"1h\"]\n\
             introduced_in = \"0.1.0\"\n{extra}\n"
        )
    }

    fn reg(metrics: &str) -> Registry {
        toml::from_str(&format!("{PRELUDE}\n{metrics}")).expect("fixture parses")
    }

    fn snap(r: &Registry) -> Snapshot {
        let rendered = crate::snapshot::render(&crate::snapshot::from_registry(r));
        toml::from_str(&rendered).expect("snapshot round-trips")
    }

    #[test]
    fn a_reused_column_name_inside_the_registry_is_fatal() {
        let r = reg(&format!(
            "{}{}",
            metric("a", "shared", 4, ""),
            metric("b", "shared", 5, "")
        ));
        let e = registry_internal(&r).unwrap_err();
        assert!(
            e.contains("column name 'shared' is allocated to two registry entries"),
            "{e}"
        );
    }

    #[test]
    fn a_reused_field_number_inside_the_registry_is_fatal() {
        let r = reg(&format!(
            "{}{}",
            metric("a", "ca", 4, ""),
            metric("b", "cb", 4, "")
        ));
        let e = registry_internal(&r).unwrap_err();
        assert!(
            e.contains("protobuf field number 4 is allocated to two"),
            "{e}"
        );
    }

    #[test]
    fn a_field_number_moving_to_another_metric_is_fatal() {
        let before = reg(&format!(
            "{}{}",
            metric("a", "ca", 4, ""),
            metric("b", "cb", 5, "")
        ));
        let s = snap(&before);
        // 'b' now claims the number that was allocated to 'a'.
        let after = reg(&format!(
            "{}{}",
            metric("a", "ca", 6, ""),
            metric("b", "cb", 4, "")
        ));
        let e = against_snapshot(&after, &s, true).unwrap_err();
        assert!(
            e.contains("protobuf field number 4 was allocated to 'a'"),
            "{e}"
        );
    }

    #[test]
    fn deleting_a_metric_without_deprecating_it_is_fatal() {
        let before = reg(&format!(
            "{}{}",
            metric("a", "ca", 4, ""),
            metric("b", "cb", 5, "")
        ));
        let s = snap(&before);
        let after = reg(&metric("a", "ca", 4, ""));
        let e = against_snapshot(&after, &s, true).unwrap_err();
        assert!(
            e.contains("is in the snapshot and gone from the registry"),
            "{e}"
        );
        assert!(e.contains("deprecated = true"), "{e}");
    }

    #[test]
    fn deprecating_a_metric_keeps_its_identifiers_and_is_not_fatal() {
        let before = reg(&format!(
            "{}{}",
            metric("a", "ca", 4, ""),
            metric("b", "cb", 5, "")
        ));
        let s = snap(&before);
        let after = reg(&format!(
            "{}{}",
            metric("a", "ca", 4, ""),
            metric("b", "cb", 5, "deprecated = true\ndeprecated_in = \"0.2.0\"")
        ));
        let changes = against_snapshot(&after, &s, false).expect("deprecation is allowed");
        assert!(changes.iter().any(|c| c.class == Class::Deprecate));
    }

    #[test]
    fn renaming_a_column_is_refused_without_the_flag_and_allowed_with_it() {
        let before = reg(&metric("a", "ca", 4, ""));
        let s = snap(&before);
        let after = reg(&metric("a", "cb", 4, ""));
        let e = against_snapshot(&after, &s, false).unwrap_err();
        assert!(e.contains("REFUSED"), "{e}");
        assert!(e.contains("--allow-breaking"), "{e}");
        against_snapshot(&after, &s, true).expect("the flag permits it");
    }

    #[test]
    fn a_metric_at_an_upper_level_but_not_the_level_below_is_fatal() {
        let r = reg(
            &metric("a", "ca", 4, "").replace("levels = [\"1m\", \"1h\"]", "levels = [\"1h\"]")
        );
        let e = registry_internal(&r).unwrap_err();
        assert!(e.contains("carried at level '1h' but not at '1m'"), "{e}");
    }

    #[test]
    fn an_aggregate_on_a_text_column_is_fatal() {
        let r =
            reg(&metric("a", "ca", 4, "").replace("sql_type = \"float4\"", "sql_type = \"text\""));
        let e = registry_internal(&r).unwrap_err();
        assert!(e.contains("raw-only"), "{e}");
    }

    #[test]
    fn one_legacy_json_key_cannot_come_from_two_metrics() {
        let r = reg(&format!(
            "{}{}",
            metric("a", "ca", 4, "legacy_rest_keys = [\"otemp\"]"),
            metric("b", "cb", 5, "legacy_rest_keys = [\"otemp\"]")
        ));
        let e = registry_internal(&r).unwrap_err();
        assert!(e.contains("claimed by 2 metrics"), "{e}");
    }

    #[test]
    fn an_over_budget_aggregate_is_fatal_before_postgresql_ever_sees_it() {
        // 400 metrics x 4 aggregate expressions = 1600, two past the 1598 a continuous
        // aggregate can hold once its bucket and partition columns are counted.
        let mut body = String::new();
        for i in 0..400 {
            body.push_str(
                &metric(&format!("m{i}"), &format!("c{i}"), 1000 + i, "").replace(
                    "aggregates = [\"mean\"]",
                    "aggregates = [\"mean\", \"min\", \"max\"]",
                ),
            );
        }
        let r = reg(&body);
        registry_internal(&r).expect("the registry itself is well formed");
        let empty = Snapshot {
            schema_version: 1,
            entries: vec![],
        };
        let e = against_snapshot(&r, &empty, true).unwrap_err();
        assert!(e.contains("would emit 1600 aggregate expressions"), "{e}");
        assert!(e.contains("over the ceiling of 1598"), "{e}");
        eprintln!("{e}");
    }

    #[test]
    fn an_over_budget_raw_table_is_fatal() {
        let mut body = String::new();
        for i in 0..1600 {
            body.push_str(&metric(&format!("m{i}"), &format!("c{i}"), 1000 + i, ""));
        }
        let r = reg(&body);
        let empty = Snapshot {
            schema_version: 1,
            entries: vec![],
        };
        let e = against_snapshot(&r, &empty, true).unwrap_err();
        assert!(
            e.contains("allocated columns, over the effective ceiling of 1595"),
            "{e}"
        );
        eprintln!("{e}");
    }

    #[test]
    fn the_real_registry_is_inside_every_budget() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let r = Registry::load(&root.join("metrics/registry.toml")).expect("loads");
        registry_internal(&r).expect("valid");
        let s = Snapshot::load(&root.join("metrics/snapshot.toml")).expect("loads");
        let b = budget(&r, Some(&s));
        assert!(b.allocated_columns <= RAW_COLUMN_CEILING);
        for (_, _, exprs) in &b.per_level {
            assert!(*exprs <= AGG_EXPRESSION_CEILING);
        }
    }
}
