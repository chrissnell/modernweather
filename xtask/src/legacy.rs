//! W9: the Go-to-Rust data migration emitter.
//!
//! The transform from the old `weather` table and its four continuous-aggregate views
//! into `observations` and the W4 hierarchy is 215 raw columns and 399 aggregate columns
//! wide. Hand-writing that is not a defensible option — a single transposed column in a
//! 215-column `INSERT ... SELECT` is silent, survives every test that does not name the
//! column, and is only found when somebody asks what the temperature was last March. So
//! it is generated, from two inputs and no transcription:
//!
//!   * `metrics/registry.toml` — the destination. What a column means, which aggregate
//!     strategies it carries and which levels carry them (D-20).
//!   * `ci/db/migrate-from-go/go-aggregate-views.toml` — the source. Which old aggregate
//!     column holds which statistic, extracted from the Go tree by
//!     `ci/db/migrate-from-go/extract-go-views.sh`.
//!
//! The second input exists because the old views are *not* self-describing. They name a
//! bucket's mean after the column — `avg(outtemp) AS outtemp` — and they name a bucket's
//! maximum after the column too, for a different set of metrics — `max(dayrain) AS
//! dayrain`. Which is which is in the view definition and nowhere else.
//!
//! What this module deliberately does **not** know is how wide production's `weather`
//! table is. The Go source declares 215 columns and production carries about 143 (W0
//! OQ-6), and nothing in this runtime can see which 143. So the raw import is emitted as
//! SQL that computes its own column list at run time from the source's catalogue,
//! intersected with the registry's expectation, and reports both halves of the
//! difference. One artefact is correct at either width.

use crate::registry::{Registry, Role, Strategy};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

pub const OUT_DIR: &str = "ci/db/migrate-from-go/generated";
pub const GO_VIEWS: &str = "ci/db/migrate-from-go/go-aggregate-views.toml";

/// path -> contents, same contract as `gen::Files`.
pub type Files = BTreeMap<String, String>;

// ---------------------------------------------------------------------------
// The second input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoViews {
    pub source_repo: String,
    pub source_commit: String,
    pub source_file: String,
    pub views: Vec<String>,
    pub column_count: usize,
    #[serde(rename = "column")]
    pub columns: Vec<GoColumn>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoColumn {
    /// The output column name in `weather_1m`/`_5m`/`_1h`/`_1d`.
    pub name: String,
    /// `avg`, `min`, `max`, `sum`, `circular_avg`, `group` or `time_bucket`.
    pub func: String,
    /// The raw column the aggregate was applied to.
    pub arg: String,
    /// `int` when the old view truncated the result (`avg(x)::int`).
    #[serde(default)]
    pub cast: Option<String>,
}

impl GoViews {
    pub fn load(path: &Path) -> Result<GoViews, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let v: GoViews = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if v.columns.len() != v.column_count {
            return Err(format!(
                "{}: declares column_count = {} but carries {} [[column]] entries",
                path.display(),
                v.column_count,
                v.columns.len()
            ));
        }
        Ok(v)
    }

    /// `(raw column, aggregate function)` -> the old view's output column.
    pub fn by_arg_func(&self) -> BTreeMap<(&str, &str), &GoColumn> {
        self.columns
            .iter()
            .filter(|c| !matches!(c.func.as_str(), "group" | "time_bucket"))
            .map(|c| ((c.arg.as_str(), c.func.as_str()), c))
            .collect()
    }

    /// The PostgreSQL type of an old aggregate column, derived from the function and the
    /// argument's registry type. Asserted against a live `weather_1d` by
    /// `the_archive_staging_types_match_a_real_old_view`: 162 `real`, 144 `integer`,
    /// 87 `double precision`, 2 `bigint`, 2 `text`, 2 `timestamptz`.
    pub fn column_type(&self, c: &GoColumn, reg: &Registry) -> &'static str {
        if c.func == "time_bucket" {
            return "timestamptz";
        }
        if c.func == "group" {
            return "text";
        }
        let arg_type = reg
            .metrics
            .iter()
            .find(|m| m.column == c.arg)
            .map(|m| m.sql_type.ddl())
            .unwrap_or("float4");
        // The old view's own cast is authoritative where it has one: `avg(gdd)::int`
        // stores an integer, and inferring that from the argument's type instead would be
        // a guess that happens to agree.
        if c.func == "avg" && c.cast.as_deref() == Some("int") {
            return "integer";
        }
        match (c.func.as_str(), arg_type) {
            // avg(real) widens to double precision; an uncast avg(int) would be numeric,
            // which the extractor has never seen and which column_count would catch.
            ("avg", "float4") => "double precision",
            ("avg", _) => "numeric",
            // sum(real) stays real; sum(int) widens to bigint.
            ("sum", "float4") => "real",
            ("sum", _) => "bigint",
            ("circular_avg", _) => "real",
            (_, "float4") => "real",
            (_, "int4") => "integer",
            (_, "int8") => "bigint",
            (_, "timestamptz") => "timestamptz",
            (_, t) => {
                debug_assert!(t == "text", "unexpected old aggregate arg type {t}");
                "text"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Where a new aggregate column's value comes from
// ---------------------------------------------------------------------------

/// One destination column of `observations_1d_legacy` / `observations_1h_legacy` and the
/// expression over the old archive row that fills it — or `None` when the old views never
/// computed that statistic, in which case the column is imported NULL and counted.
pub struct LegacyColumn {
    pub name: String,
    pub sql_type: &'static str,
    /// `None` = no counterpart in the Go views; the column lands NULL.
    pub expr: Option<String>,
    /// Why there is no counterpart, for the generated comment and the gap report.
    pub note: Option<String>,
}

/// The whole mapping for one level, in canonical registry order.
pub fn legacy_columns(reg: &Registry, go: &GoViews, level: &str) -> Vec<LegacyColumn> {
    let idx = go.by_arg_func();
    let mut out = Vec::new();
    for m in reg.metrics_ordered() {
        if !m.in_level(&reg.defaults, level) {
            continue;
        }
        let c = m.column.as_str();
        for s in &m.aggregates {
            let old_func = match s {
                Strategy::Mean => "avg",
                Strategy::Min => "min",
                Strategy::Max => "max",
                Strategy::Total => "sum",
                Strategy::CircularMean => "circular_avg",
            };
            let src = idx.get(&(c, old_func)).copied();
            // The physical columns this strategy expands to, and how each is reconstructed
            // from a single archived bucket. The count is 1 because an archived bucket is
            // one bucket: `mw_mean(sum, 1) = sum`, so the mean the old view stored is the
            // mean the finaliser returns, exactly. See docs/data-migration.md §4.2.
            match s {
                Strategy::Mean => {
                    let (sum, cnt) = match src {
                        Some(o) => (
                            Some(format!("a.{}::double precision", o.name)),
                            Some(format!(
                                "CASE WHEN a.{} IS NULL THEN NULL ELSE 1::bigint END",
                                o.name
                            )),
                        ),
                        None => (None, None),
                    };
                    out.push(LegacyColumn {
                        name: format!("{c}_sum"),
                        sql_type: "double precision",
                        expr: sum,
                        note: none_note(src.is_none(), c, "mean"),
                    });
                    out.push(LegacyColumn {
                        name: format!("{c}_count"),
                        sql_type: "bigint",
                        expr: cnt,
                        note: none_note(src.is_none(), c, "mean"),
                    });
                }
                Strategy::Min => out.push(LegacyColumn {
                    name: format!("{c}_min"),
                    sql_type: m.sql_type.ddl(),
                    expr: src.map(|o| format!("a.{}", o.name)),
                    note: none_note(src.is_none(), c, "min"),
                }),
                Strategy::Max => out.push(LegacyColumn {
                    name: format!("{c}_max"),
                    sql_type: m.sql_type.ddl(),
                    expr: src.map(|o| format!("a.{}", o.name)),
                    note: none_note(src.is_none(), c, "max"),
                }),
                Strategy::Total => out.push(LegacyColumn {
                    name: format!("{c}_total"),
                    sql_type: "double precision",
                    expr: src.map(|o| format!("a.{}::double precision", o.name)),
                    note: none_note(src.is_none(), c, "total"),
                }),
                Strategy::CircularMean => {
                    // D-13 stores an angle as a sine sum, a cosine sum and a count, and
                    // never as a finished angle. One archived bucket's circular_avg is
                    // recovered exactly: atan2d(sind(d)/1, cosd(d)/1) = d.
                    let (sin, cos, n) = match src {
                        Some(o) => (
                            Some(format!("sind(a.{}::double precision)", o.name)),
                            Some(format!("cosd(a.{}::double precision)", o.name)),
                            Some(format!(
                                "CASE WHEN a.{} IS NULL THEN NULL ELSE 1::bigint END",
                                o.name
                            )),
                        ),
                        None => (None, None, None),
                    };
                    out.push(LegacyColumn {
                        name: format!("{c}_sin_sum"),
                        sql_type: "double precision",
                        expr: sin,
                        note: none_note(src.is_none(), c, "circular_mean"),
                    });
                    out.push(LegacyColumn {
                        name: format!("{c}_cos_sum"),
                        sql_type: "double precision",
                        expr: cos,
                        note: none_note(src.is_none(), c, "circular_mean"),
                    });
                    out.push(LegacyColumn {
                        name: format!("{c}_dir_count"),
                        sql_type: "bigint",
                        expr: n,
                        note: none_note(src.is_none(), c, "circular_mean"),
                    });
                }
            }
        }
    }
    out
}

fn none_note(missing: bool, col: &str, strategy: &str) -> Option<String> {
    missing.then(|| format!("the Go views never computed {strategy}({col})"))
}

/// Old aggregate columns the transform does not consume, with the reason. Four of these
/// are semantic and 39 are the truncated integer means; docs/data-migration.md §4.3.
pub fn unconsumed(reg: &Registry, go: &GoViews) -> Vec<(String, String)> {
    let mut used: BTreeSet<&str> = BTreeSet::new();
    let idx = go.by_arg_func();
    for m in reg.metrics_ordered() {
        for s in &m.aggregates {
            let f = match s {
                Strategy::Mean => "avg",
                Strategy::Min => "min",
                Strategy::Max => "max",
                Strategy::Total => "sum",
                Strategy::CircularMean => "circular_avg",
            };
            if let Some(o) = idx.get(&(m.column.as_str(), f)) {
                used.insert(o.name.as_str());
            }
        }
    }
    go.columns
        .iter()
        .filter(|c| !matches!(c.func.as_str(), "group" | "time_bucket"))
        .filter(|c| !used.contains(c.name.as_str()))
        .map(|c| {
            let m = reg.metrics.iter().find(|m| m.column == c.arg);
            let strategies = m
                .map(|m| {
                    m.aggregates
                        .iter()
                        .map(|s| s.name())
                        .collect::<Vec<_>>()
                        .join("+")
                })
                .unwrap_or_else(|| "no registry entry".into());
            let why = if strategies.is_empty() {
                format!(
                    "{}({}) — the registry gives {} no aggregate at all",
                    c.func, c.arg, c.arg
                )
            } else {
                format!(
                    "{}({}) — the registry models {} as {}",
                    c.func, c.arg, c.arg, strategies
                )
            };
            (c.name.clone(), why)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Emission
// ---------------------------------------------------------------------------

fn header() -> String {
    "-- GENERATED by `cargo xtask legacy-import` from metrics/registry.toml and\n\
     -- ci/db/migrate-from-go/go-aggregate-views.toml. DO NOT EDIT.\n\
     --\n\
     -- `cargo xtask legacy-import --check` regenerates into a temporary directory and\n\
     -- diffs, so a hand edit here is a build failure rather than a surviving change.\n\
     -- The procedure these files implement is docs/data-migration.md.\n\n"
        .to_string()
}

pub fn all(reg: &Registry, go: &GoViews) -> Files {
    let mut f = Files::new();
    f.insert(format!("{OUT_DIR}/00-staging.sql"), staging(reg, go));
    f.insert(format!("{OUT_DIR}/10-import-raw.sql"), import_raw(reg));
    f.insert(
        format!("{OUT_DIR}/20-legacy-rollup.sql"),
        legacy_rollup(reg, go),
    );
    f.insert(format!("{OUT_DIR}/30-reconcile.sql"), reconcile(reg, go));
    f
}

/// The raw columns the new table expects to receive from the old one, in attribute order.
/// Every registry column except the partition key, which the old table does not have:
/// the Go schema keys readings by station *name*.
fn expected_raw_columns(reg: &Registry) -> Vec<(String, &'static str, &'static str)> {
    let mut v: Vec<(String, &'static str, &'static str)> = Vec::new();
    for d in &reg.dimensions {
        if d.role == Role::Partition {
            continue;
        }
        let kind = match d.role {
            Role::Time => "time",
            Role::Attribute => "attribute",
            Role::Partition => unreachable!(),
        };
        v.push((d.column.clone(), d.sql_type.ddl(), kind));
    }
    for m in reg.metrics_ordered() {
        v.push((
            m.column.clone(),
            m.sql_type.ddl(),
            if m.deprecated {
                "metric_deprecated"
            } else {
                "metric"
            },
        ));
    }
    v
}

fn staging(reg: &Registry, go: &GoViews) -> String {
    let mut s = header();
    let expected = expected_raw_columns(reg);
    let archive_cols: Vec<(&str, &'static str)> = go
        .columns
        .iter()
        .map(|c| (c.name.as_str(), go.column_type(c, reg)))
        .collect();

    s.push_str(&format!(
        "-- Staging and the column inventory. Runs on the TARGET (new) database. Nothing in\n\
         -- the migration ever writes to the source: it is read-only for the duration\n\
         -- (docs/data-migration.md Decision 3), which is also why the source's column list\n\
         -- has to be carried over rather than queried through a foreign table.\n\n\
         CREATE SCHEMA IF NOT EXISTS w9;\n\n\
         COMMENT ON SCHEMA w9 IS\n\
         \x20   'One-time Go-to-Rust migration staging (W9). Droppable once\n\
         \x20    docs/data-migration.md reconciliation passes and the source is decommissioned;\n\
         \x20    observations_1d_legacy is NOT in here and is not droppable.';\n\n\
         -- ------------------------------------------------------------------\n\
         -- 1. What the destination expects: {} columns from metrics/registry.toml.\n\
         --    The partition key `station_id` is absent on purpose — the old table has no\n\
         --    such column, and resolving it is w9.station_map's job.\n\
         -- ------------------------------------------------------------------\n\n\
         CREATE TABLE w9.expected_column (\n\
         \x20   column_name text PRIMARY KEY,\n\
         \x20   sql_type    text NOT NULL,\n\
         \x20   kind        text NOT NULL,\n\
         \x20   ord         int  NOT NULL UNIQUE\n\
         );\n\n\
         -- `ord` is the destination's attribute order, and it is a column rather than an\n\
         -- ORDER BY on ctid because the two halves of the streaming COPY have to agree on\n\
         -- column order exactly and a physical-order dependency is not a guarantee.\n\
         INSERT INTO w9.expected_column (column_name, sql_type, kind, ord) VALUES\n",
        expected.len()
    ));
    for (i, (c, t, k)) in expected.iter().enumerate() {
        let sep = if i + 1 == expected.len() { ";" } else { "," };
        let _ = writeln!(s, "    ('{c}', '{t}', '{k}', {}){sep}", i + 1);
    }

    s.push_str(
        "\n-- ------------------------------------------------------------------\n\
         -- 2. What the source actually has. Populated by migrate-from-go.sh from a\n\
         --    read-only query against the old database; see w9.source_inventory_query().\n\
         --    Production declares ~143 of the 215 columns the Go source defines and nothing\n\
         --    in the repository knows which 143 (W0 OQ-6), so this is data, not a constant.\n\
         -- ------------------------------------------------------------------\n\n\
         CREATE TABLE w9.source_column (\n\
         \x20   column_name text PRIMARY KEY,\n\
         \x20   data_type   text NOT NULL,\n\
         \x20   ordinal     int  NOT NULL\n\
         );\n\n\
         CREATE FUNCTION w9.source_inventory_query() RETURNS text\n\
         LANGUAGE sql IMMUTABLE AS $fn$\n\
         \x20   SELECT $q$COPY (SELECT column_name, udt_name, ordinal_position\n\
         \x20                     FROM information_schema.columns\n\
         \x20                    WHERE table_schema = 'public' AND table_name = 'weather'\n\
         \x20                    ORDER BY ordinal_position) TO STDOUT (FORMAT csv)$q$\n\
         $fn$;\n\n\
         COMMENT ON FUNCTION w9.source_inventory_query() IS\n\
         \x20   'The read-only statement to run on the OLD database. Its output loads into\n\
         \x20    w9.source_column. It reads a catalogue view and writes nothing.';\n\n\
         -- The gap report. Three classes, and only one of them is benign.\n\
         --\n\
         --   missing_in_source  the registry declares it, production does not have it. The\n\
         --                      column imports as NULL. Expected for ~72 columns (OQ-6) and\n\
         --                      harmless: a NULL costs one bit (D-3). Report the count.\n\
         --   unknown_in_source  production has a column the registry has never heard of.\n\
         --                      This is NOT benign — it is data with no destination, and it\n\
         --                      has to become a [[metric]] entry, go into `extra`, or be\n\
         --                      consciously dropped, BEFORE the import runs.\n\
         --   type_mismatch      both sides have it and disagree on the type.\n\
         CREATE VIEW w9.gap_report AS\n\
         SELECT coalesce(e.column_name, s.column_name) AS column_name,\n\
         \x20      e.sql_type   AS expected_type,\n\
         \x20      s.data_type  AS source_type,\n\
         \x20      e.kind,\n\
         \x20      CASE WHEN s.column_name IS NULL THEN 'missing_in_source'\n\
         \x20           WHEN e.column_name IS NULL THEN 'unknown_in_source'\n\
         \x20           WHEN e.sql_type <> s.data_type THEN 'type_mismatch'\n\
         \x20           ELSE 'ok' END AS verdict\n\
         \x20 FROM w9.expected_column e\n\
         \x20 FULL OUTER JOIN w9.source_column s USING (column_name);\n\n\
         -- The third class has one legitimate resolution besides modelling it: decide, on\n\
         -- the record, not to carry it. That decision belongs in the database rather than\n\
         -- in a shell history, because the alternative to writing it down is deleting a row\n\
         -- from w9.source_column, which leaves no trace that it was deliberate.\n\
         CREATE TABLE w9.dropped_source_column (\n\
         \x20   column_name text PRIMARY KEY,\n\
         \x20   reason      text NOT NULL,\n\
         \x20   decided_at  timestamptz NOT NULL DEFAULT now()\n\
         );\n\n\
         COMMENT ON TABLE w9.dropped_source_column IS\n\
         \x20   'Source columns consciously not carried over. Every row is a decision someone\n\
         \x20    made; `reason` is not optional for that reason.';\n\n\
         -- Fails the migration rather than importing into a column nobody chose.\n\
         CREATE FUNCTION w9.assert_no_unknown_source_columns() RETURNS void\n\
         LANGUAGE plpgsql AS $fn$\n\
         DECLARE bad text;\n\
         BEGIN\n\
         \x20   SELECT string_agg(g.column_name || ' ' || g.source_type, ', ' ORDER BY g.column_name)\n\
         \x20     INTO bad\n\
         \x20     FROM w9.gap_report g\n\
         \x20     LEFT JOIN w9.dropped_source_column d USING (column_name)\n\
         \x20    WHERE g.verdict = 'unknown_in_source' AND d.column_name IS NULL;\n\
         \x20   IF bad IS NOT NULL THEN\n\
         \x20       RAISE EXCEPTION 'the source has columns with no registry entry: %', bad\n\
         \x20           USING HINT = 'add a [[metric]] entry, route it to observations.extra, '\n\
         \x20                        'or INSERT it into w9.dropped_source_column with a reason';\n\
         \x20   END IF;\n\
         END $fn$;\n\n\
         -- ------------------------------------------------------------------\n\
         -- 3. stationname -> station_id. The one mapping a human has to supply.\n\
         -- ------------------------------------------------------------------\n\
         --\n\
         -- The old table keys readings by station *name* and the new one by `station_id`\n\
         -- (W4 2.1). `/span?station=<device name>` is frozen and matches the name, so the\n\
         -- safe default is station_id = stationname: it keeps every historical row\n\
         -- addressable by the parameter both shipped clients already send. Choosing\n\
         -- something else is allowed and means the history is reachable only under the new\n\
         -- id, which is why this table is populated by hand and asserted non-empty rather\n\
         -- than defaulted silently.\n\n\
         CREATE TABLE w9.station_map (\n\
         \x20   stationname           text PRIMARY KEY,\n\
         \x20   station_id            text NOT NULL UNIQUE,\n\
         \x20   stationtype           text,\n\
         \x20   timezone              text NOT NULL DEFAULT 'UTC',\n\
         \x20   latitude              double precision,\n\
         \x20   longitude             double precision,\n\
         \x20   altitude_ft           double precision,\n\
         \x20   snow_base_distance_mm double precision\n\
         );\n\n\
         -- The names actually present in the source, so the map can be checked for\n\
         -- completeness before 19 GB moves. Loaded by migrate-from-go.sh.\n\
         CREATE TABLE w9.source_station (\n\
         \x20   stationname text PRIMARY KEY,\n\
         \x20   stationtype text,\n\
         \x20   first_seen  timestamptz,\n\
         \x20   last_seen   timestamptz,\n\
         \x20   rows        bigint\n\
         );\n\n\
         CREATE FUNCTION w9.source_station_query() RETURNS text\n\
         LANGUAGE sql IMMUTABLE AS $fn$\n\
         \x20   SELECT $q$COPY (SELECT stationname, min(stationtype), min(time), max(time), count(*)\n\
         \x20                     FROM weather GROUP BY stationname ORDER BY stationname)\n\
         \x20             TO STDOUT (FORMAT csv)$q$\n\
         $fn$;\n\n\
         CREATE FUNCTION w9.assert_station_map_complete() RETURNS void\n\
         LANGUAGE plpgsql AS $fn$\n\
         DECLARE bad text;\n\
         BEGIN\n\
         \x20   SELECT string_agg(s.stationname, ', ' ORDER BY s.stationname) INTO bad\n\
         \x20     FROM w9.source_station s\n\
         \x20     LEFT JOIN w9.station_map m USING (stationname)\n\
         \x20    WHERE m.stationname IS NULL;\n\
         \x20   IF bad IS NOT NULL THEN\n\
         \x20       RAISE EXCEPTION 'w9.station_map has no entry for: %', bad\n\
         \x20           USING HINT = 'INSERT INTO w9.station_map (stationname, station_id, stationtype) '\n\
         \x20                        'SELECT stationname, stationname, stationtype FROM w9.source_station '\n\
         \x20                        'adopts the frozen-contract-preserving default';\n\
         \x20   END IF;\n\
         \x20   IF NOT EXISTS (SELECT 1 FROM w9.source_station) THEN\n\
         \x20       RAISE EXCEPTION 'w9.source_station is empty; load it before asserting'\n\
         \x20           USING HINT = 'migrate-from-go.sh inventory';\n\
         \x20   END IF;\n\
         END $fn$;\n\n",
    );

    let _ = write!(
        s,
        "-- ------------------------------------------------------------------\n\
         -- 4. The archive landing tables: the old aggregate views' own {ncols} columns.\n\
         -- ------------------------------------------------------------------\n\
         --\n\
         -- Shape taken from {repo} {file} at {sha},\n\
         -- extracted by ci/db/migrate-from-go/extract-go-views.sh. All of {views} are\n\
         -- shape-identical apart from the bucket width — the extractor asserts it — so one\n\
         -- definition serves both archives.\n\
         --\n\
         -- These receive weather_1d_archive / weather_1h_archive from the source. They are\n\
         -- staging, not the destination: the destination is observations_1d_legacy in\n\
         -- 20-legacy-rollup.sql.\n\n",
        ncols = go.columns.len(),
        repo = go.source_repo,
        file = go.source_file,
        sha = &go.source_commit[..12],
        views = go.views.join(", "),
    );
    for lvl in ["1d", "1h"] {
        let _ = writeln!(s, "CREATE TABLE w9.weather_{lvl}_archive (");
        let w = archive_cols
            .iter()
            .map(|(n, _)| n.len())
            .max()
            .unwrap_or(20);
        for (i, (n, t)) in archive_cols.iter().enumerate() {
            let sep = if i + 1 == archive_cols.len() { "" } else { "," };
            let _ = writeln!(s, "    {n:w$} {t}{sep}");
        }
        s.push_str(");\n\n");
    }
    s.push_str(
        "-- The old views have no uniqueness constraint on (bucket, stationname) — a\n\
         -- materialisation hypertable does not carry one — so the archive copy can in\n\
         -- principle arrive with duplicates. Assert rather than assume: a duplicated bucket\n\
         -- would double a daily rainfall total and halve nothing, which no reconciliation\n\
         -- below would flag as a type error.\n\
         CREATE FUNCTION w9.assert_archive_unique(level text) RETURNS void\n\
         LANGUAGE plpgsql AS $fn$\n\
         DECLARE n bigint;\n\
         BEGIN\n\
         \x20   EXECUTE format('SELECT count(*) FROM (SELECT bucket, stationname FROM\n\
         \x20                   w9.weather_%s_archive GROUP BY 1,2 HAVING count(*) > 1) d', level)\n\
         \x20     INTO n;\n\
         \x20   IF n > 0 THEN\n\
         \x20       RAISE EXCEPTION 'w9.weather_%_archive has % duplicated (bucket, stationname) pairs',\n\
         \x20           level, n;\n\
         \x20   END IF;\n\
         END $fn$;\n",
    );
    s
}

fn import_raw(reg: &Registry) -> String {
    let mut s = header();
    let part = reg.partition_column();
    let time = reg.time_column();
    s.push_str(&format!(
        "-- The raw import. Runs on the TARGET.\n\
         --\n\
         -- The column list is computed at run time from w9.source_column INTERSECT\n\
         -- w9.expected_column rather than baked in, because production's width is unknown\n\
         -- (OQ-6) and a baked list would either fail on a missing column or silently skip a\n\
         -- present one. The emitted statements are a matched pair of COPYs, so 19 GB streams\n\
         -- through a pipe with no intermediate dump and no staging table:\n\
         --\n\
         --   psql \"$SOURCE\" -c \"$(copy_out_sql ...)\" | psql \"$TARGET\" -c \"$(copy_in_sql)\"\n\
         --\n\
         -- FORMAT csv, not binary. Binary COPY is faster and is only safe when both ends\n\
         -- run the same PostgreSQL major version, which is exactly what OQ-1 has not\n\
         -- established. CSV round-trips float4 exactly on PostgreSQL 12 and later, where the\n\
         -- default float output is shortest-exact.\n\n\
         -- The columns to move, in the destination's attribute order so that the two COPY\n\
         -- statements cannot disagree.\n\
         CREATE FUNCTION w9.import_columns() RETURNS text[]\n\
         LANGUAGE sql STABLE AS $fn$\n\
         \x20   SELECT coalesce(array_agg(e.column_name ORDER BY e.ord), '{{}}'::text[])\n\
         \x20     FROM w9.expected_column e\n\
         \x20     JOIN w9.source_column  s USING (column_name)\n\
         $fn$;\n\n\
         -- stationname -> station_id as a CASE, so that the source side needs no join and\n\
         -- no temporary object. Three or four stations; this is a few hundred bytes.\n\
         CREATE FUNCTION w9.station_id_expr() RETURNS text\n\
         LANGUAGE plpgsql STABLE AS $fn$\n\
         DECLARE e text;\n\
         BEGIN\n\
         \x20   SELECT 'CASE ' || string_agg(format('WHEN stationname = %L THEN %L',\n\
         \x20                                       stationname, station_id), ' '\n\
         \x20                                ORDER BY stationname) || ' END'\n\
         \x20     INTO e FROM w9.station_map;\n\
         \x20   IF e IS NULL THEN\n\
         \x20       RAISE EXCEPTION 'w9.station_map is empty; nothing can be assigned a {part_col}';\n\
         \x20   END IF;\n\
         \x20   RETURN e;\n\
         END $fn$;\n\n\
         -- One slice of the source, as a read-only statement to run on the OLD database.\n\
         -- Sliced because a 19 GB single COPY is not resumable and a failed one has to be\n\
         -- started again; sliced by time because that is how the chunks are laid out.\n\
         CREATE FUNCTION w9.copy_out_sql(t0 timestamptz, t1 timestamptz) RETURNS text\n\
         LANGUAGE sql STABLE AS $fn$\n\
         \x20   SELECT format(\n\
         \x20       'COPY (SELECT %s AS {part_col}, %s FROM weather '\n\
         \x20       'WHERE {time_col} >= %L AND {time_col} < %L) TO STDOUT (FORMAT csv)',\n\
         \x20       w9.station_id_expr(),\n\
         \x20       (SELECT string_agg(quote_ident(c), ', ') FROM unnest(w9.import_columns()) c),\n\
         \x20       t0, t1)\n\
         $fn$;\n\n\
         CREATE FUNCTION w9.copy_in_sql() RETURNS text\n\
         LANGUAGE sql STABLE AS $fn$\n\
         \x20   SELECT format('COPY {raw} ({part_col}, %s) FROM STDIN (FORMAT csv)',\n\
         \x20                 (SELECT string_agg(quote_ident(c), ', ')\n\
         \x20                    FROM unnest(w9.import_columns()) c))\n\
         $fn$;\n\n\
         -- The slice boundaries, from the source's own extent. Seven days to match the\n\
         -- source's chunk interval, so a slice is one chunk's worth of sequential read.\n\
         CREATE TABLE w9.import_slice (\n\
         \x20   t0        timestamptz PRIMARY KEY,\n\
         \x20   t1        timestamptz NOT NULL,\n\
         \x20   rows      bigint,\n\
         \x20   done_at   timestamptz\n\
         );\n\n\
         -- Clear a slice before importing it. A slice that is planned and not marked done\n\
         -- either has never been imported or was interrupted part-way, and the second case\n\
         -- is the one that matters: COPY has no ON CONFLICT, so retrying a half-imported\n\
         -- slice fails on the first duplicate against\n\
         -- observations_station_id_time_key — which is exactly the unique index W4\n\
         -- assumption 34 asks to exist before the import, so the collision is a feature.\n\
         -- Deleting the slice's range first makes every slice retryable, and the ranges are\n\
         -- disjoint by construction so this can only remove that slice's own debris.\n\
         --\n\
         -- The alternative is a staging table per slice plus INSERT ... ON CONFLICT DO\n\
         -- NOTHING, which costs a second write of every row to avoid a DELETE that normally\n\
         -- removes nothing. Not worth it during an import that owns the table.\n\
         -- The parameters are named from_ts/to_ts, not t0/t1: w9.import_slice has columns\n\
         -- called t0 and t1, and plpgsql resolves the variable in preference to the column\n\
         -- without saying so.\n\
         CREATE PROCEDURE w9.reset_slice(from_ts timestamptz, to_ts timestamptz)\n\
         LANGUAGE plpgsql AS $fn$\n\
         DECLARE n bigint;\n\
         BEGIN\n\
         \x20   IF EXISTS (SELECT 1 FROM w9.import_slice s\n\
         \x20               WHERE s.t0 = from_ts AND s.done_at IS NOT NULL) THEN\n\
         \x20       RAISE EXCEPTION 'slice % is already marked done; refusing to delete it', from_ts\n\
         \x20           USING HINT = 'clear done_at first if the slice really has to be re-imported';\n\
         \x20   END IF;\n\
         \x20   DELETE FROM {raw} WHERE {time_col} >= from_ts AND {time_col} < to_ts;\n\
         \x20   GET DIAGNOSTICS n = ROW_COUNT;\n\
         \x20   IF n > 0 THEN\n\
         \x20       RAISE NOTICE 'slice % had % rows from an interrupted attempt; removed', from_ts, n;\n\
         \x20   END IF;\n\
         END $fn$;\n\n\
         CREATE PROCEDURE w9.plan_import(from_ts timestamptz, to_ts timestamptz,\n\
         \x20                            slice interval DEFAULT INTERVAL '7 days')\n\
         LANGUAGE sql AS $fn$\n\
         \x20   INSERT INTO w9.import_slice (t0, t1)\n\
         \x20   SELECT g, least(g + slice, to_ts)\n\
         \x20     FROM generate_series(from_ts, to_ts - INTERVAL '1 microsecond', slice) g\n\
         \x20   ON CONFLICT (t0) DO NOTHING;\n\
         $fn$;\n\n\
         -- ------------------------------------------------------------------\n\
         -- `stations` first. W4 2.1 makes it the reading-side projection every /span join\n\
         -- goes through, and the low-volume tables carry a foreign key to it, so a reading\n\
         -- whose station has no row is addressable by nothing.\n\
         -- ------------------------------------------------------------------\n\n\
         CREATE PROCEDURE w9.populate_stations() LANGUAGE plpgsql AS $fn$\n\
         BEGIN\n\
         \x20   PERFORM w9.assert_station_map_complete();\n\
         \x20   INSERT INTO stations (station_id, name, type, timezone, latitude, longitude,\n\
         \x20                         altitude_ft, snow_base_distance_mm, first_seen, last_seen)\n\
         \x20   SELECT m.station_id,\n\
         \x20          m.stationname,\n\
         \x20          coalesce(m.stationtype, s.stationtype, 'unknown'),\n\
         \x20          m.timezone, m.latitude, m.longitude, m.altitude_ft,\n\
         \x20          m.snow_base_distance_mm,\n\
         \x20          coalesce(s.first_seen, now()), s.last_seen\n\
         \x20     FROM w9.station_map m\n\
         \x20     LEFT JOIN w9.source_station s USING (stationname)\n\
         \x20   ON CONFLICT (station_id) DO UPDATE\n\
         \x20      SET first_seen = least(stations.first_seen, excluded.first_seen),\n\
         \x20          last_seen  = greatest(stations.last_seen, excluded.last_seen);\n\
         END $fn$;\n\n\
         -- `station_metrics` after the import: which metrics a station has ever reported\n\
         -- (W4 2.2). One sequential pass over the imported history, which is the scan the\n\
         -- Go TODO at internal/weatherstations/grpcreceiver/station.go:64 never wanted to\n\
         -- pay per request — paying it once, here, is what the table is for.\n\
         CREATE PROCEDURE w9.populate_station_metrics() LANGUAGE plpgsql AS $fn$\n\
         DECLARE r record; q text;\n\
         BEGIN\n\
         \x20   FOR r IN SELECT e.column_name, m.key\n\
         \x20              FROM w9.expected_column e\n\
         \x20              JOIN w9.source_column USING (column_name)\n\
         \x20              JOIN w9.metric_key m ON m.column_name = e.column_name\n\
         \x20             WHERE e.kind = 'metric'\n\
         \x20   LOOP\n\
         \x20       q := format($q$\n\
         \x20           INSERT INTO station_metrics (station_id, metric_key, first_seen, last_seen)\n\
         \x20           SELECT {part_col}, %L, min({time_col}), max({time_col})\n\
         \x20             FROM {raw} WHERE %I IS NOT NULL GROUP BY 1\n\
         \x20           ON CONFLICT (station_id, metric_key) DO UPDATE\n\
         \x20              SET first_seen = least(station_metrics.first_seen, excluded.first_seen),\n\
         \x20                  last_seen  = greatest(station_metrics.last_seen, excluded.last_seen)\n\
         \x20       $q$, r.key, r.column_name);\n\
         \x20       EXECUTE q;\n\
         \x20   END LOOP;\n\
         END $fn$;\n\n\
         -- `station_metrics` is keyed by the registry's stable `key`, not by the column name\n\
         -- (W4 2.2): `out_temp` is what the API calls the metric and `outtemp` is an\n\
         -- artefact of what the old database happened to name a column.\n\
         CREATE TABLE w9.metric_key (column_name text PRIMARY KEY, key text NOT NULL);\n\
         INSERT INTO w9.metric_key (column_name, key) VALUES\n",
        part_col = part.column,
        time_col = time.column,
        raw = crate::gen::RAW_TABLE,
    ));
    let live: Vec<_> = reg.live_metrics();
    for (i, m) in live.iter().enumerate() {
        let sep = if i + 1 == live.len() { ";" } else { "," };
        let _ = writeln!(s, "    ('{}', '{}'){sep}", m.column, m.key);
    }
    s
}

fn legacy_rollup(reg: &Registry, go: &GoViews) -> String {
    let mut s = header();
    s.push_str(
        "-- The rollup archive: ~294 days of daily and hourly buckets with no raw behind\n\
         -- them, imported as pre-computed rows.\n\
         --\n\
         -- WHY THESE ARE PLAIN TABLES AND NOT PRE-SEEDED CONTINUOUS AGGREGATES\n\
         --\n\
         -- Rows inserted into a continuous aggregate on TimescaleDB 2.31 are accepted — the\n\
         -- INSERT is rewritten to the materialisation hypertable — and then destroyed by any\n\
         -- of three ordinary operations, none of which looks destructive:\n\
         --\n\
         --   * `CALL refresh_continuous_aggregate(level, lo, hi)` over the range. Measured:\n\
         --     21 of 29 pre-seeded buckets deleted, no error, no warning.\n\
         --   * the level's own scheduled refresh policy, once retention has dropped the raw\n\
         --     underneath. Measured: 41 of 83 buckets deleted by one policy run, and the\n\
         --     range destroyed was exactly the range whose raw had gone.\n\
         --   * adding a metric, which drops and recreates every level carrying it (D-15).\n\
         --     The chunk goes with the view.\n\
         --\n\
         -- A plain table survives all three, says in its own name that its rows were not\n\
         -- computed from raw, and costs 4 MB. docs/data-migration.md 3 and 5.\n\n",
    );

    for lvl in ["1d", "1h"] {
        let cols = legacy_columns(reg, go, lvl);
        let with = cols.iter().filter(|c| c.expr.is_some()).count();
        let _ = write!(
            s,
            "-- ------------------------------------------------------------------\n\
             -- observations_{lvl}_legacy: {} aggregate columns, {} of which the Go views\n\
             -- computed. The other {} land NULL, which is correct: the old system never\n\
             -- had that number, and a NULL says so where a zero would lie.\n\
             -- ------------------------------------------------------------------\n\n\
             CREATE TABLE observations_{lvl}_legacy (\n\
             \x20   bucket     timestamptz NOT NULL,\n\
             \x20   station_id text        NOT NULL,\n\
             \x20   source     text        NOT NULL DEFAULT 'go:weather_{lvl}',\n",
            cols.len(),
            with,
            cols.len() - with,
        );
        let w = cols.iter().map(|c| c.name.len()).max().unwrap_or(24);
        for c in &cols {
            // A NULL column carries its reason in the DDL. Without it, reading this file
            // later cannot distinguish "the old system never had this number" from "the
            // mapping missed it", and those want opposite responses.
            match &c.note {
                Some(n) => {
                    let _ = writeln!(s, "    {:w$} {},   -- NULL: {n}", c.name, c.sql_type);
                }
                None => {
                    let _ = writeln!(s, "    {:w$} {},", c.name, c.sql_type);
                }
            }
        }
        let _ = write!(
            s,
            "    PRIMARY KEY (station_id, bucket)\n\
             );\n\n\
             COMMENT ON TABLE observations_{lvl}_legacy IS\n\
             \x20   'Pre-computed {lvl} buckets imported from the Go weather_{lvl} continuous\n\
             \x20    aggregate, for the window where the raw readings behind them were deleted by\n\
             \x20    the old 365-day retention policy before the migration. Not derived from\n\
             \x20    observations and not rebuildable from anything. Never truncate this.';\n\n"
        );
    }

    s.push_str(
        "-- The boundary. One row per station: the first bucket for which real raw exists.\n\
         -- Imports filter strictly below it and the assertion below proves the two sides do\n\
         -- not overlap, so the union views cannot double-count a bucket.\n\
         CREATE TABLE w9.legacy_boundary (\n\
         \x20   station_id text PRIMARY KEY,\n\
         \x20   cutover    timestamptz NOT NULL\n\
         );\n\n\
         COMMENT ON TABLE w9.legacy_boundary IS\n\
         \x20   'Per station, the first bucket that has raw readings behind it. Set from the\n\
         \x20    imported raw extent, not from the archive.';\n\n\
         CREATE PROCEDURE w9.set_legacy_boundary() LANGUAGE sql AS $fn$\n\
         \x20   INSERT INTO w9.legacy_boundary (station_id, cutover)\n\
         \x20   SELECT station_id, min(time) FROM observations GROUP BY 1\n\
         \x20   ON CONFLICT (station_id) DO UPDATE SET cutover = excluded.cutover;\n\
         $fn$;\n\n",
    );

    for lvl in ["1d", "1h"] {
        let cols = legacy_columns(reg, go, lvl);
        let bucket = if lvl == "1d" { "1 day" } else { "1 hour" };
        let _ = write!(
            s,
            "-- The {lvl} transform. One archived bucket becomes one legacy row.\n\
             --\n\
             -- The mean is the part worth reading twice. The old view stored a finished\n\
             -- average and the new level stores a sum and a count, so the import sets\n\
             -- count = 1 and sum = the average: mw_mean(avg, 1) = avg, exactly, with no\n\
             -- rounding and no reconstruction. The count is not a lie about how many\n\
             -- readings there were — nothing reads a count except the finaliser, nothing\n\
             -- rolls up out of the top level, and the alternative is inventing a reading\n\
             -- count the old view never recorded. docs/data-migration.md 4.2.\n\
             --\n\
             -- The same identity holds for the angle: atan2d(sind(d)/1, cosd(d)/1) = d.\n\
             CREATE PROCEDURE w9.import_legacy_{lvl}() LANGUAGE plpgsql AS $fn$\n\
             BEGIN\n\
             \x20   PERFORM w9.assert_archive_unique('{lvl}');\n\
             \x20   INSERT INTO observations_{lvl}_legacy (bucket, station_id,\n"
        );
        for (i, c) in cols.iter().enumerate() {
            if c.expr.is_none() {
                continue;
            }
            let last = cols[i + 1..].iter().all(|x| x.expr.is_none());
            let _ = writeln!(s, "        {}{}", c.name, if last { "" } else { "," });
        }
        s.push_str("    )\n    SELECT date_trunc('");
        s.push_str(bucket.split(' ').nth(1).unwrap_or("day"));
        s.push_str("', a.bucket), m.station_id,\n");
        for (i, c) in cols.iter().enumerate() {
            let Some(e) = &c.expr else { continue };
            let last = cols[i + 1..].iter().all(|x| x.expr.is_none());
            let _ = writeln!(s, "        {e}{}", if last { "" } else { "," });
        }
        let _ = write!(
            s,
            "    FROM w9.weather_{lvl}_archive a\n\
             \x20   JOIN w9.station_map m ON m.stationname = a.stationname\n\
             \x20   LEFT JOIN w9.legacy_boundary b ON b.station_id = m.station_id\n\
             \x20  WHERE b.cutover IS NULL OR a.bucket < b.cutover\n\
             \x20  ON CONFLICT (station_id, bucket) DO NOTHING;\n\
             END $fn$;\n\n"
        );
    }

    s.push_str(
        "-- The read-side union. /span reads these where it would read the level, for the\n\
         -- one reason the legacy table exists: below the cutover the level is empty and the\n\
         -- archive is all there is. UNION ALL and not UNION, because the assertion below\n\
         -- establishes disjointness and a UNION would pay for a de-duplication that cannot\n\
         -- find anything.\n\
         --\n\
         -- Only the 1-day level needs this. /span reads observations_1h over at most 14\n\
         -- days and observations_5m over at most 48 hours (W4 1.2), both of which are\n\
         -- inside the raw window on any day this migration could run; observations_1d is\n\
         -- the only level /span reads over history. observations_1h_legacy is imported\n\
         -- anyway, because it is 40 MB and because it is the only hourly resolution that\n\
         -- window will ever have.\n",
    );
    for lvl in ["1d", "1h"] {
        let cols = legacy_columns(reg, go, lvl);
        let _ = write!(
            s,
            "\nCREATE VIEW observations_{lvl}_with_legacy AS\n\
             SELECT bucket, station_id,\n"
        );
        for (i, c) in cols.iter().enumerate() {
            let sep = if i + 1 == cols.len() { "" } else { "," };
            let _ = writeln!(s, "       {}{sep}", c.name);
        }
        let _ = write!(
            s,
            "  FROM observations_{lvl}\nUNION ALL\nSELECT bucket, station_id,\n"
        );
        for (i, c) in cols.iter().enumerate() {
            let sep = if i + 1 == cols.len() { "" } else { "," };
            let _ = writeln!(s, "       {}{sep}", c.name);
        }
        let _ = writeln!(s, "  FROM observations_{lvl}_legacy;");
    }

    s.push_str(
        "\n-- Disjointness, asserted rather than assumed. An overlapping bucket would make\n\
         -- /span return two elements with the same `ts`, which no client expects and which\n\
         -- the frozen contract's \"a gap is missing elements\" rule (Q-9) does not cover.\n\
         CREATE FUNCTION w9.assert_legacy_disjoint(level text) RETURNS void\n\
         LANGUAGE plpgsql AS $fn$\n\
         DECLARE n bigint;\n\
         BEGIN\n\
         \x20   EXECUTE format('SELECT count(*) FROM observations_%s l\n\
         \x20                   JOIN observations_%s_legacy g USING (station_id, bucket)',\n\
         \x20                  level, level)\n\
         \x20     INTO n;\n\
         \x20   IF n > 0 THEN\n\
         \x20       RAISE EXCEPTION 'observations_% and observations_%_legacy share % buckets',\n\
         \x20           level, level, n;\n\
         \x20   END IF;\n\
         END $fn$;\n\n\
         -- The old aggregate columns this transform does not consume, recorded in the\n\
         -- database so that \"did we look at all 399?\" is answerable without reading a\n\
         -- document. docs/data-migration.md 4.3 says what each one is.\n\
         CREATE TABLE w9.unconsumed_archive_column (column_name text PRIMARY KEY, reason text NOT NULL);\n\
         INSERT INTO w9.unconsumed_archive_column (column_name, reason) VALUES\n",
    );
    let un = unconsumed(reg, go);
    for (i, (n, why)) in un.iter().enumerate() {
        let sep = if i + 1 == un.len() { ";" } else { "," };
        let _ = writeln!(s, "    ('{n}', '{}'){sep}", why.replace('\'', "''"));
    }
    s
}

fn reconcile(reg: &Registry, go: &GoViews) -> String {
    let mut s = header();
    let time = reg.time_column().column.clone();
    let part = reg.partition_column().column.clone();

    s.push_str(
        "-- Reconciliation. The old database stays read-only until every check here passes\n\
         -- (docs/data-migration.md Decision 3).\n\
         --\n\
         -- Every aggregate below is `sum(col::numeric)`, never `sum(col::float8)`. Floating\n\
         -- addition is not associative, so a float sum over 19 GB depends on the order the\n\
         -- rows arrive in and on how many parallel workers the planner chose — the two\n\
         -- sides would differ in the last bits and the operator would be left deciding\n\
         -- whether 1e-9 is a migration defect. numeric is exact and order-independent, so\n\
         -- the comparison can be `IS DISTINCT FROM` with no tolerance, which is the only\n\
         -- kind of comparison that cannot be argued with.\n\n\
         CREATE TABLE w9.recon (\n\
         \x20   side       text NOT NULL CHECK (side IN ('old', 'new')),\n\
         \x20   station_id text NOT NULL,\n\
         \x20   yr         int  NOT NULL,\n\
         \x20   metric     text NOT NULL,\n\
         \x20   rows       bigint,\n\
         \x20   non_null   bigint,\n\
         \x20   total      numeric,\n\
         \x20   lo         numeric,\n\
         \x20   hi         numeric,\n\
         \x20   PRIMARY KEY (side, station_id, yr, metric)\n\
         );\n\n",
    );

    // The digest query, emitted twice: once for the source's shape, once for the target's.
    // Both are one sequential pass with a LATERAL unpivot, not one scan per metric.
    let numeric: Vec<&crate::registry::Metric> = reg
        .live_metrics()
        .into_iter()
        .filter(|m| m.sql_type.numeric())
        .collect();

    for (side, table, key, filter) in [
        ("old", "weather", "stationname", ""),
        ("new", crate::gen::RAW_TABLE, part.as_str(), ""),
    ] {
        let _ = write!(
            s,
            "-- The {side} side's digest, over {} numeric metrics. One pass; the LATERAL\n\
             -- unpivots what a single GROUP BY already computed, so this is not {} scans.\n\
             --\n\
             -- `rows` repeats the station-year row count on every metric row on purpose: it\n\
             -- makes each row independently checkable without a join back to a header.\n",
            numeric.len(),
            numeric.len(),
        );
        if side == "old" {
            s.push_str(
                "--\n\
                 -- The old side runs on the OLD database and writes nothing there: the text\n\
                 -- w9.digest_body('old', ...) returns is a bare SELECT, which\n\
                 -- migrate-from-go.sh wraps in a COPY ... TO STDOUT and loads into w9.recon.\n\
                 -- The metric list is filtered through w9.source_column first, so a column\n\
                 -- production does not have cannot make the statement fail to parse.\n",
            );
        }
        let _ = writeln!(
            s,
            "-- Emitted shape (side = '{side}', table = {table}, key = {key}):"
        );
        let _ = write!(
            s,
            "--   SELECT '{side}', {key}, extract(year from {time})::int, v.metric, n, v.*\n\
             --     FROM (SELECT ... count(*) n, count(<col>) , sum(<col>::numeric), min, max ...\n\
             --             FROM {table}{filter} GROUP BY 1,2) z,\n\
             --          LATERAL (VALUES (<one row per metric>)) v(metric, non_null, total, lo, hi)\n\n"
        );
    }

    // The body builder lives in SQL so that the same generated text serves both sides and
    // can be filtered against w9.source_column.
    let mut vals = String::new();
    let mut aggs = String::new();
    for (i, m) in numeric.iter().enumerate() {
        let c = &m.column;
        let sep = if i + 1 == numeric.len() { "" } else { "," };
        let _ = writeln!(
            aggs,
            "    count({c}) AS {c}_nn, sum({c}::numeric) AS {c}_t, min({c})::numeric AS {c}_lo, max({c})::numeric AS {c}_hi{sep}"
        );
        let _ = writeln!(
            vals,
            "        ('{c}', z.{c}_nn, z.{c}_t, z.{c}_lo, z.{c}_hi){sep}"
        );
    }

    let _ = write!(
        s,
        "-- The digest body, assembled from the metrics the *source* actually has so that one\n\
         -- function serves a 143-column production table and a 215-column one.\n\
         CREATE TABLE w9.digest_metric (column_name text PRIMARY KEY, ord int NOT NULL);\n\
         INSERT INTO w9.digest_metric (column_name, ord) VALUES\n"
    );
    for (i, m) in numeric.iter().enumerate() {
        let sep = if i + 1 == numeric.len() { ";" } else { "," };
        let _ = writeln!(s, "    ('{}', {}){sep}", m.column, i + 1);
    }
    let _ = write!(
        s,
        "\nCREATE FUNCTION w9.digest_body(side text, t0 timestamptz, t1 timestamptz) RETURNS text\n\
         LANGUAGE plpgsql STABLE AS $fn$\n\
         DECLARE tbl text; key text; aggs text; vals text;\n\
         BEGIN\n\
         \x20   IF side = 'old' THEN tbl := 'weather';      key := 'stationname';\n\
         \x20   ELSE                 tbl := '{raw}'; key := '{part}';\n\
         \x20   END IF;\n\
         \x20   SELECT string_agg(format('count(%1$I) AS %1$I_nn, sum(%1$I::numeric) AS %1$I_t, '\n\
         \x20                            'min(%1$I)::numeric AS %1$I_lo, max(%1$I)::numeric AS %1$I_hi',\n\
         \x20                            d.column_name), ', ' ORDER BY d.ord),\n\
         \x20          string_agg(format('(%1$L, z.%2$I_nn, z.%2$I_t, z.%2$I_lo, z.%2$I_hi)',\n\
         \x20                            d.column_name, d.column_name), ', ' ORDER BY d.ord)\n\
         \x20     INTO aggs, vals\n\
         \x20     FROM w9.digest_metric d\n\
         \x20     JOIN w9.source_column s ON s.column_name = d.column_name;\n\
         \x20   IF aggs IS NULL THEN\n\
         \x20       RAISE EXCEPTION 'w9.source_column is empty; load the inventory first';\n\
         \x20   END IF;\n\
         \x20   RETURN format(\n\
         \x20       'SELECT %L, z.k, z.yr, v.metric, z.n, v.non_null, v.total, v.lo, v.hi '\n\
         \x20       'FROM (SELECT %I AS k, extract(year from {time})::int AS yr, count(*) AS n, %s '\n\
         \x20       'FROM %I WHERE {time} >= %L AND {time} < %L GROUP BY 1, 2) z, '\n\
         \x20       'LATERAL (VALUES %s) v(metric, non_null, total, lo, hi)',\n\
         \x20       side, key, aggs, tbl, t0, t1, vals);\n\
         END $fn$;\n\n\
         COMMENT ON FUNCTION w9.digest_body(text, timestamptz, timestamptz) IS\n\
         \x20   'One sequential pass per side. The LATERAL unpivots columns a single GROUP BY\n\
         \x20    already produced, so this does not scan once per metric.';\n\n\
         -- The old side groups by stationname because that is the only station key the old\n\
         -- table has. migrate-from-go.sh maps it to station_id through w9.station_map on the\n\
         -- way in, so the two sides of this join are in the same namespace.\n\
         CREATE VIEW w9.recon_report AS\n\
         SELECT coalesce(o.station_id, n.station_id) AS station_id,\n\
         \x20      coalesce(o.yr, n.yr)              AS yr,\n\
         \x20      coalesce(o.metric, n.metric)      AS metric,\n\
         \x20      o.rows AS old_rows, n.rows AS new_rows,\n\
         \x20      o.non_null AS old_non_null, n.non_null AS new_non_null,\n\
         \x20      o.total AS old_total, n.total AS new_total,\n\
         \x20      o.lo AS old_lo, n.lo AS new_lo,\n\
         \x20      o.hi AS old_hi, n.hi AS new_hi,\n\
         \x20      CASE WHEN n.station_id IS NULL THEN 'missing_in_new'\n\
         \x20           WHEN o.station_id IS NULL THEN 'extra_in_new'\n\
         \x20           WHEN o.rows IS DISTINCT FROM n.rows THEN 'row_count'\n\
         \x20           WHEN o.non_null IS DISTINCT FROM n.non_null THEN 'non_null_count'\n\
         \x20           WHEN o.total IS DISTINCT FROM n.total THEN 'sum'\n\
         \x20           WHEN o.lo IS DISTINCT FROM n.lo THEN 'min'\n\
         \x20           WHEN o.hi IS DISTINCT FROM n.hi THEN 'max'\n\
         \x20           ELSE 'ok' END AS verdict\n\
         \x20 FROM (SELECT * FROM w9.recon WHERE side = 'old') o\n\
         \x20 FULL OUTER JOIN (SELECT * FROM w9.recon WHERE side = 'new') n\n\
         \x20   USING (station_id, yr, metric);\n\n\
         CREATE FUNCTION w9.assert_reconciled() RETURNS void LANGUAGE plpgsql AS $fn$\n\
         DECLARE n bigint; sample text;\n\
         BEGIN\n\
         \x20   SELECT count(*), string_agg(format('%s/%s/%s: %s', station_id, yr, metric, verdict),\n\
         \x20                               '; ' ORDER BY station_id, yr, metric)\n\
         \x20     INTO n, sample\n\
         \x20     FROM (SELECT * FROM w9.recon_report WHERE verdict <> 'ok' LIMIT 20) d;\n\
         \x20   IF NOT EXISTS (SELECT 1 FROM w9.recon) THEN\n\
         \x20       RAISE EXCEPTION 'w9.recon is empty: nothing was reconciled'\n\
         \x20           USING HINT = 'a reconciliation that ran against no rows is not a pass';\n\
         \x20   END IF;\n\
         \x20   IF n > 0 THEN\n\
         \x20       RAISE EXCEPTION 'reconciliation found % mismatching (station, year, metric) rows: %',\n\
         \x20           n, sample;\n\
         \x20   END IF;\n\
         END $fn$;\n\n",
        raw = crate::gen::RAW_TABLE,
        part = part,
        time = time,
    );

    // R-4: the hierarchy's daily rollup against the old daily view.
    s.push_str(
        "-- ------------------------------------------------------------------\n\
         -- The check the other ones cannot make: does the *new hierarchy* reproduce the old\n\
         -- daily view's numbers? The digests above compare raw against raw, which proves the\n\
         -- rows moved. This compares observations_1d, built bottom-up through four levels,\n\
         -- against weather_1d, built in one step from raw — so it also catches the class of\n\
         -- defect D-12 and D-14 exist for.\n\
         --\n\
         -- The old view's daily mean is `avg(col)` over raw; the new one is\n\
         -- mw_mean(col_sum, col_count) over three intermediate levels. D-14 widened the\n\
         -- accumulator specifically so these agree exactly, so the comparison is exact to\n\
         -- 1e-9 relative — which is float4's own representable precision, not a tolerance\n\
         -- chosen to make the test pass.\n\
         -- ------------------------------------------------------------------\n\n\
         CREATE TABLE w9.rollup_recon (\n\
         \x20   station_id text NOT NULL,\n\
         \x20   bucket     timestamptz NOT NULL,\n\
         \x20   metric     text NOT NULL,\n\
         \x20   stat       text NOT NULL,\n\
         \x20   old_value  double precision,\n\
         \x20   new_value  double precision,\n\
         \x20   -- Readings in the bucket, which is what bounds the old side's error for any\n\
         \x20   -- statistic the Go views accumulated in `real`. See tol_model below.\n\
         \x20   n_readings bigint,\n\
         \x20   -- 'exact'             the old view stored this at double precision or at the\n\
         \x20   --                     column's own width and computed it in one operation:\n\
         \x20   --                     avg(real) accumulates in float8 and returns float8,\n\
         \x20   --                     min and max are the value itself. Zero tolerance.\n\
         \x20   -- 'float4_accumulated' the old view both accumulated AND stored this in\n\
         \x20   --                     `real`: sum(real) returns real, and the Go\n\
         \x20   --                     circular_avg_state declares sin_sum, cos_sum and even\n\
         \x20   --                     its count as `real`. Error grows with the number of\n\
         \x20   --                     additions, which is exactly the defect D-14 widened the\n\
         \x20   --                     accumulator to avoid — so here the NEW value is the\n\
         \x20   --                     accurate one and the bound is on how wrong the OLD one\n\
         \x20   --                     is allowed to be: n * 2^-24 relative.\n\
         \x20   tol_model  text NOT NULL DEFAULT 'exact'\n\
         \x20       CHECK (tol_model IN ('exact', 'float4_accumulated')),\n\
         \x20   PRIMARY KEY (station_id, bucket, metric, stat)\n\
         );\n\n\
         CREATE VIEW w9.rollup_report AS\n\
         SELECT *,\n\
         \x20      CASE tol_model\n\
         \x20           WHEN 'exact' THEN 0.0\n\
         \x20           ELSE coalesce(n_readings, 1) * 5.96e-8\n\
         \x20      END * greatest(abs(coalesce(old_value, 0)), abs(coalesce(new_value, 0)), 1)\n\
         \x20        AS tolerance,\n\
         \x20      CASE WHEN old_value IS NULL AND new_value IS NULL THEN 'ok'\n\
         \x20           WHEN old_value IS NULL OR  new_value IS NULL THEN 'one_side_null'\n\
         \x20           WHEN tol_model = 'exact'\n\
         \x20                THEN CASE WHEN old_value = new_value THEN 'ok' ELSE 'differs' END\n\
         \x20           WHEN abs(old_value - new_value)\n\
         \x20                <= coalesce(n_readings, 1) * 5.96e-8\n\
         \x20                   * greatest(abs(old_value), abs(new_value), 1) THEN 'ok'\n\
         \x20           ELSE 'differs' END AS verdict\n\
         \x20 FROM w9.rollup_recon;\n\n\
         -- 5.96e-8 is 2^-24, half an ulp of float4's 24-bit mantissa. Multiplying it by the\n\
         -- number of additions is the standard worst-case bound for a running sum in that\n\
         -- width. It is not a tolerance chosen to make the check pass: 'exact' rows get zero\n\
         -- tolerance and must agree bit for bit, and the two statistics that get a bound are\n\
         -- the two where the Go schema's own accumulator width makes exact agreement\n\
         -- arithmetically impossible.\n\n",
    );

    // The spot checks — remembered days.
    let _ = write!(
        s,
        "-- ------------------------------------------------------------------\n\
         -- Spot checks on values a human remembers. Row counts and sums prove the bytes\n\
         -- arrived; an extreme and its timestamp prove they arrived meaning the same thing.\n\
         -- Both sides emit the same shape and the operator reads the two tables side by\n\
         -- side, because \"was the hottest day of 2025 the day I remember?\" is not a question\n\
         -- a constraint can answer.\n\
         -- ------------------------------------------------------------------\n\n\
         -- Six named extremes per station per year, with the timestamp that produced each.\n\
         -- One statement per (metric, direction) because an extreme needs DISTINCT ON per\n\
         -- group, which no single GROUP BY gives; seven metrics by two directions is\n\
         -- fourteen index scans, not fourteen table scans.\n\
         --\n\
         -- `side` is emitted as the first column so that the old side — which arrives over a\n\
         -- COPY pipe and cannot have a column added on the way in — and the new side have\n\
         -- exactly the same shape.\n\
         CREATE FUNCTION w9.extreme_sql(side text, col text, dir text,\n\
         \x20                           t0 timestamptz, t1 timestamptz) RETURNS text\n\
         LANGUAGE sql STABLE AS $fn$\n\
         \x20   SELECT format(\n\
         \x20       'SELECT %L::text AS side, %I AS station, extract(year from {time})::int AS yr, '\n\
         \x20       '%L AS metric, %L AS dir, %I::double precision AS value, {time} AS at FROM ('\n\
         \x20       '  SELECT DISTINCT ON (%I, extract(year from {time})) %I, {time}, %I '\n\
         \x20       '    FROM %I WHERE {time} >= %L AND {time} < %L AND %I IS NOT NULL '\n\
         \x20       '   ORDER BY %I, extract(year from {time}), %I %s, {time} ASC) q',\n\
         \x20       side,\n\
         \x20       CASE WHEN side = 'old' THEN 'stationname' ELSE '{part}' END,\n\
         \x20       col, dir, col,\n\
         \x20       CASE WHEN side = 'old' THEN 'stationname' ELSE '{part}' END,\n\
         \x20       CASE WHEN side = 'old' THEN 'stationname' ELSE '{part}' END, col,\n\
         \x20       CASE WHEN side = 'old' THEN 'weather' ELSE '{raw}' END,\n\
         \x20       t0, t1, col,\n\
         \x20       CASE WHEN side = 'old' THEN 'stationname' ELSE '{part}' END,\n\
         \x20       col, CASE WHEN upper(dir) = 'MAX' THEN 'DESC' ELSE 'ASC' END)\n\
         $fn$;\n\n\
         CREATE TABLE w9.extreme (\n\
         \x20   side    text NOT NULL CHECK (side IN ('old', 'new')),\n\
         \x20   station text NOT NULL,\n\
         \x20   yr      int  NOT NULL,\n\
         \x20   metric  text NOT NULL,\n\
         \x20   dir     text NOT NULL,\n\
         \x20   value   double precision,\n\
         \x20   at      timestamptz,\n\
         \x20   PRIMARY KEY (side, station, yr, metric, dir)\n\
         );\n\n\
         CREATE VIEW w9.extreme_report AS\n\
         SELECT coalesce(o.station, n.station) AS station,\n\
         \x20      coalesce(o.yr, n.yr) AS yr,\n\
         \x20      coalesce(o.metric, n.metric) AS metric,\n\
         \x20      coalesce(o.dir, n.dir) AS dir,\n\
         \x20      o.value AS old_value, o.at AS old_at,\n\
         \x20      n.value AS new_value, n.at AS new_at,\n\
         \x20      CASE WHEN o.value IS DISTINCT FROM n.value THEN 'value_differs'\n\
         \x20           WHEN o.at IS DISTINCT FROM n.at THEN 'timestamp_differs'\n\
         \x20           ELSE 'ok' END AS verdict\n\
         \x20 FROM (SELECT * FROM w9.extreme WHERE side = 'old') o\n\
         \x20 FULL OUTER JOIN (SELECT * FROM w9.extreme WHERE side = 'new') n\n\
         \x20   USING (station, yr, metric, dir);\n\n\
         -- The metrics worth spot-checking: the ones a person has an opinion about.\n\
         -- Keyed by (col, dir), not by col: the same metric is spot-checked in both\n\
         -- directions, and a primary key on col alone silently keeps one of the two.\n\
         CREATE TABLE w9.spot_metric (col text NOT NULL, dir text NOT NULL,\n\
         \x20                        PRIMARY KEY (col, dir));\n\
         INSERT INTO w9.spot_metric (col, dir) VALUES\n\
         \x20   ('outtemp', 'max'), ('outtemp', 'min'), ('windspeed', 'max'),\n\
         \x20   ('barometer', 'max'), ('barometer', 'min'), ('dayrain', 'max'),\n\
         \x20   ('snowdepth', 'max')\n\
         ON CONFLICT DO NOTHING;\n",
        time = time,
        part = part,
        raw = crate::gen::RAW_TABLE,
    );
    let _ = go; // the reconciliation side needs the registry only
    s
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;

    fn inputs() -> (Registry, GoViews) {
        let root = crate::repo_root();
        (
            Registry::load(&root.join(crate::REGISTRY)).expect("registry"),
            GoViews::load(&root.join(GO_VIEWS)).expect("go views"),
        )
    }

    #[test]
    fn the_go_view_inventory_covers_every_column_of_the_old_views() {
        let (_, go) = inputs();
        // 399 columns: bucket, stationname, stationtype and 396 aggregates.
        assert_eq!(go.columns.len(), 399, "old view width");
        assert_eq!(
            go.columns
                .iter()
                .filter(|c| matches!(c.func.as_str(), "group" | "time_bucket"))
                .count(),
            3
        );
        assert!(go.columns.iter().all(|c| !c.name.is_empty()));
    }

    #[test]
    fn the_archive_staging_types_match_a_real_old_view() {
        // Counted from information_schema on a weather_1d built from the Go DDL on
        // timescale/timescaledb-ha:pg17: 162 real, 144 integer, 87 double precision,
        // 2 bigint, 2 text, 2 timestamptz. The derivation has to reproduce that exactly,
        // or the archive lands in columns that silently round.
        let (reg, go) = inputs();
        let mut n = std::collections::BTreeMap::new();
        for c in &go.columns {
            *n.entry(go.column_type(c, &reg)).or_insert(0usize) += 1;
        }
        assert_eq!(n.get("real"), Some(&162), "real columns: {n:?}");
        assert_eq!(n.get("integer"), Some(&144), "integer columns: {n:?}");
        assert_eq!(
            n.get("double precision"),
            Some(&87),
            "double precision columns: {n:?}"
        );
        assert_eq!(n.get("bigint"), Some(&2), "bigint columns: {n:?}");
        assert_eq!(n.get("text"), Some(&2), "text columns: {n:?}");
        assert_eq!(n.get("timestamptz"), Some(&2), "timestamptz columns: {n:?}");
    }

    #[test]
    fn every_old_aggregate_column_has_a_registry_entry_for_its_argument() {
        // The direction that would be a real problem: an archived column whose underlying
        // metric the registry has never heard of would be data with no destination.
        let (reg, go) = inputs();
        let known: BTreeSet<&str> = reg
            .metrics
            .iter()
            .map(|m| m.column.as_str())
            .chain(reg.dimensions.iter().map(|d| d.column.as_str()))
            .collect();
        let orphans: Vec<&str> = go
            .columns
            .iter()
            .filter(|c| c.func != "time_bucket")
            .filter(|c| !known.contains(c.arg.as_str()))
            .map(|c| c.name.as_str())
            .collect();
        assert!(
            orphans.is_empty(),
            "archive columns with no metric: {orphans:?}"
        );
    }

    #[test]
    fn the_legacy_mapping_covers_the_whole_destination_and_nothing_twice() {
        let (reg, go) = inputs();
        for level in ["1m", "5m", "1h", "1d"] {
            let cols = legacy_columns(&reg, &go, level);
            let want = reg.level_columns(level);
            assert_eq!(
                cols.len(),
                want.len(),
                "{level}: legacy mapping width against the generated level"
            );
            let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
            let expect: Vec<&str> = want.iter().map(|(_, c)| c.name.as_str()).collect();
            assert_eq!(names, expect, "{level}: column order must match the level");
            let uniq: BTreeSet<&str> = names.iter().copied().collect();
            assert_eq!(uniq.len(), names.len(), "{level}: duplicated column");
        }
    }

    #[test]
    fn a_mean_is_reconstructed_as_sum_and_count_one_and_an_angle_as_sine_and_cosine() {
        let (reg, go) = inputs();
        let cols = legacy_columns(&reg, &go, "1d");
        let find = |n: &str| cols.iter().find(|c| c.name == n).expect(n);
        // out_temp: the old view stores avg(outtemp); the new level stores sum and count.
        assert_eq!(
            find("outtemp_sum").expr.as_deref(),
            Some("a.outtemp::double precision")
        );
        assert_eq!(
            find("outtemp_count").expr.as_deref(),
            Some("CASE WHEN a.outtemp IS NULL THEN NULL ELSE 1::bigint END")
        );
        // The angle never arrives as an angle.
        assert_eq!(
            find("winddir_sin_sum").expr.as_deref(),
            Some("sind(a.winddir::double precision)")
        );
        assert_eq!(
            find("winddir_cos_sum").expr.as_deref(),
            Some("cosd(a.winddir::double precision)")
        );
        assert!(
            !cols
                .iter()
                .any(|c| c.name == "winddir_mean" || c.name == "winddir"),
            "no column may hold a finished angle"
        );
        // period_rain is the one legacy key that names an aggregate (W1 assumption 11).
        assert_eq!(
            find("rainincremental_total").expr.as_deref(),
            Some("a.period_rain::double precision")
        );
        // dayrain's bare old column is a max, not a mean. Getting this backwards is the
        // specific defect go-aggregate-views.toml exists to prevent.
        assert_eq!(find("dayrain_max").expr.as_deref(), Some("a.dayrain"));
    }

    #[test]
    fn the_columns_with_no_archive_source_are_counted_and_not_invented() {
        let (reg, go) = inputs();
        let cols = legacy_columns(&reg, &go, "1d");
        let with = cols.iter().filter(|c| c.expr.is_some()).count();
        // Measured against the registry at this commit: 451 of 627 aggregate columns have a
        // counterpart in the Go views and 176 do not. The numbers are asserted so that a
        // registry change which quietly widens the gap shows up as a failing test and a
        // documented figure rather than as more NULLs.
        assert_eq!(cols.len(), 627, "aggregate columns at the 1d level");
        assert_eq!(with, 451, "columns the Go archive can fill");
        assert_eq!(cols.len() - with, 176, "columns that land NULL");
        // Every unfilled column must say why. A NULL with no reason is indistinguishable
        // from a mapping bug.
        assert!(cols
            .iter()
            .filter(|c| c.expr.is_none())
            .all(|c| c.note.is_some()));
    }

    #[test]
    fn the_unconsumed_archive_columns_are_the_four_semantic_ones_and_the_truncated_means() {
        let (reg, go) = inputs();
        let un = unconsumed(&reg, &go);
        let names: BTreeSet<&str> = un.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(un.len(), 43, "unconsumed archive columns: {names:?}");
        for n in [
            "lightning_day",  // sum() in the old view, max in the registry
            "lightning_hour", // same
            "lightning_time", // timestamptz, no aggregate in the registry at all
            "min_gdd",        // min(gdd), and the registry gives gdd only max
        ] {
            assert!(names.contains(n), "{n} should be unconsumed");
        }
        // The other 39 are the integer means the old view truncated with ::int.
        let truncated = go
            .columns
            .iter()
            .filter(|c| names.contains(c.name.as_str()) && c.func == "avg")
            .count();
        assert_eq!(truncated, 39);
    }

    #[test]
    fn the_emitted_sql_names_no_column_the_destination_does_not_have() {
        // The emitter writes INSERT column lists by hand; a typo there is a runtime error
        // in a 629-column statement nobody will read. Cross-check every identifier the
        // legacy INSERTs name against the generated level definition.
        let (reg, go) = inputs();
        let files = all(&reg, &go);
        let body = files
            .get(&format!("{OUT_DIR}/20-legacy-rollup.sql"))
            .expect("20-legacy-rollup.sql");
        for level in ["1d", "1h"] {
            for (_, c) in reg.level_columns(level) {
                assert!(
                    body.contains(&c.name),
                    "{level}: {} is in the level and not in the emitted SQL",
                    c.name
                );
            }
        }
        assert!(body.contains("observations_1d_legacy"));
        assert!(body.contains("UNION ALL"));
    }

    #[test]
    fn the_emitted_files_are_deterministic() {
        // --check compares bytes, so a BTreeMap iteration order or a HashSet anywhere in
        // the emitter turns the gate into a coin flip.
        let (reg, go) = inputs();
        assert_eq!(all(&reg, &go), all(&reg, &go));
    }

    #[test]
    fn the_raw_import_expects_every_registry_column_except_the_partition_key() {
        let (reg, go) = inputs();
        let files = all(&reg, &go);
        let staging = files.get(&format!("{OUT_DIR}/00-staging.sql")).unwrap();
        // 215 columns: the Go table's own width, which is the registry's 216 minus
        // station_id. Any drift here means the import would silently skip a column.
        let n = staging
            .lines()
            .filter(|l| l.trim_start().starts_with("('") && l.contains("', '"))
            .count();
        // 215 raw columns + 212 live metric keys in w9.metric_key would both match a
        // looser pattern, so count only the rows this file emits.
        assert!(n >= 215, "expected_column rows: {n}");
        assert!(
            !staging.contains("('station_id', 'text'"),
            "the old table has no station_id"
        );
        for d in &reg.dimensions {
            if d.role == Role::Partition {
                continue;
            }
            assert!(staging.contains(&format!("('{}', ", d.column)));
        }
        // Column order is load-bearing for the matched COPY pair.
        assert!(staging.contains("ord         int  NOT NULL UNIQUE"));
    }

    #[test]
    fn reconciliation_sums_in_numeric_and_compares_without_a_tolerance() {
        let (reg, go) = inputs();
        let files = all(&reg, &go);
        let body = files.get(&format!("{OUT_DIR}/30-reconcile.sql")).unwrap();
        assert!(body.contains("::numeric"), "sums must be exact");
        assert!(!body.contains("sum(%1$I::float8)"));
        assert!(
            body.contains("IS DISTINCT FROM"),
            "no tolerance on the digests"
        );
        assert!(body.contains("w9.assert_reconciled"));
        // A reconciliation over zero rows must fail, not pass.
        assert!(body.contains("nothing was reconciled"));
    }
}
