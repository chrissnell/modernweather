//! The registry model: `metrics/registry.toml` deserialised, plus the one piece of
//! derived knowledge every emitter needs — which aggregate columns a metric expands to.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub schema_version: u32,
    pub defaults: Defaults,
    #[serde(default, rename = "level")]
    pub levels: Vec<Level>,
    #[serde(default, rename = "dimension")]
    pub dimensions: Vec<Dimension>,
    #[serde(default, rename = "metric")]
    pub metrics: Vec<Metric>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub mean_representation: MeanRepr,
    pub levels: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Level {
    pub name: String,
    pub bucket: String,
    /// `"raw"` for the first level, otherwise the name of the level below it (D-11).
    pub source: String,
    /// Materialised-only, or real-time aggregation over the materialised data. W4's
    /// to tune; `true` is required of any level that is the source of another.
    #[serde(default = "default_true")]
    pub materialized_only: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dimension {
    pub key: String,
    pub column: String,
    pub sql_type: SqlType,
    pub role: Role,
    pub field_number: u32,
    pub proto_name: String,
    pub proto_type: String,
    pub label: String,
    pub introduced_in: String,
    #[serde(default)]
    pub legacy_rest_keys: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    pub key: String,
    pub column: String,
    pub sql_type: SqlType,
    pub field_number: u32,
    pub proto_name: String,
    pub proto_type: String,
    pub label: String,
    pub unit: String,
    pub tier: Tier,
    pub aggregates: Vec<Strategy>,
    /// Which aggregate levels carry this metric (OQ-5). Empty means raw only.
    #[serde(default)]
    pub levels: Option<Vec<String>>,
    #[serde(default)]
    pub legacy_rest_keys: Vec<String>,
    /// The frozen handler computes this value at read time rather than reading the
    /// column, so the column and the JSON key are not the same number. W1 owns the
    /// exact behaviour; the registry only records that the seam exists.
    #[serde(default)]
    pub legacy_read_time_computed: bool,
    /// Legacy JSON key for an *aggregate* column whose legacy name does not follow
    /// the convention. Only `rainincremental.total = ["period_rain"]` today.
    #[serde(default)]
    pub legacy_aggregate_rest_keys: BTreeMap<String, Vec<String>>,
    pub introduced_in: String,
    /// Deprecated metrics keep their identifiers forever (D-21). They stay in the
    /// raw table, leave the aggregates, and are omitted from the generated types.
    #[serde(default)]
    pub deprecated: bool,
    #[serde(default)]
    pub deprecated_in: Option<String>,
    /// Overrides `[defaults].mean_representation` for this metric.
    #[serde(default)]
    pub mean_representation: Option<MeanRepr>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The hypertable's time dimension and the aggregates' bucket source.
    Time,
    /// The aggregates' GROUP BY key.
    Partition,
    /// Present in the raw table and the reading types, absent from the aggregates.
    Attribute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Core,
    Extended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeanRepr {
    /// Two columns: `<col>_sum double precision` and `<col>_count bigint`.
    SumCount,
    /// One column: `<col>_stats` holding a Toolkit `statssummary1`.
    StatsAgg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Mean,
    Min,
    Max,
    /// Bucket total, e.g. rainfall accumulated over the bucket.
    Total,
    /// Vector mean of an angle. Three composable columns, never an arithmetic mean.
    CircularMean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlType {
    Float4,
    Int4,
    Int8,
    Text,
    Timestamptz,
}

impl SqlType {
    pub fn ddl(self) -> &'static str {
        match self {
            SqlType::Float4 => "float4",
            SqlType::Int4 => "int4",
            SqlType::Int8 => "int8",
            SqlType::Text => "text",
            SqlType::Timestamptz => "timestamptz",
        }
    }
    pub fn numeric(self) -> bool {
        matches!(self, SqlType::Float4 | SqlType::Int4 | SqlType::Int8)
    }
    pub fn rust(self) -> &'static str {
        match self {
            SqlType::Float4 => "f32",
            SqlType::Int4 => "i32",
            SqlType::Int8 => "i64",
            SqlType::Text => "String",
            SqlType::Timestamptz => "DateTime<Utc>",
        }
    }
    pub fn ts(self) -> &'static str {
        match self {
            SqlType::Text => "string",
            SqlType::Timestamptz => "string",
            _ => "number",
        }
    }
    /// OpenAPI (type, format).
    pub fn openapi(self) -> (&'static str, Option<&'static str>) {
        match self {
            SqlType::Float4 => ("number", Some("float")),
            SqlType::Int4 => ("integer", Some("int32")),
            SqlType::Int8 => ("integer", Some("int64")),
            SqlType::Text => ("string", None),
            SqlType::Timestamptz => ("string", Some("date-time")),
        }
    }
}

impl fmt::Display for SqlType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.ddl())
    }
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Strategy {
    pub fn name(self) -> &'static str {
        match self {
            Strategy::Mean => "mean",
            Strategy::Min => "min",
            Strategy::Max => "max",
            Strategy::Total => "total",
            Strategy::CircularMean => "circular_mean",
        }
    }
}

/// One physical column of one aggregate view.
///
/// `from_raw` and `from_level` are the two halves of H-3: a level reads the level
/// below it, never raw, except for the first level in the hierarchy. Both forms are
/// produced here, from the same place, so a caller cannot accidentally emit the
/// naive form — which is the point. H-1 (widened accumulator) and H-2 (nulled count)
/// are likewise properties of these strings and of nothing else.
pub struct AggColumn {
    pub name: String,
    pub sql_type: &'static str,
    pub from_raw: String,
    pub from_level: String,
    pub strategy: Strategy,
}

impl Metric {
    pub fn mean_repr(&self, defaults: &Defaults) -> MeanRepr {
        self.mean_representation
            .unwrap_or(defaults.mean_representation)
    }

    pub fn levels<'a>(&'a self, defaults: &'a Defaults) -> &'a [String] {
        match &self.levels {
            Some(l) => l,
            None => &defaults.levels,
        }
    }

    pub fn in_level(&self, defaults: &Defaults, level: &str) -> bool {
        !self.deprecated
            && !self.aggregates.is_empty()
            && self.levels(defaults).iter().any(|l| l == level)
    }

    /// Expand this metric into the aggregate columns its strategies require.
    pub fn agg_columns(&self, defaults: &Defaults) -> Vec<AggColumn> {
        let c = &self.column;
        let ty = self.sql_type.ddl();
        let mut out = Vec::new();
        for s in &self.aggregates {
            match s {
                Strategy::Mean => match self.mean_repr(defaults) {
                    MeanRepr::SumCount => {
                        out.push(AggColumn {
                            name: format!("{c}_sum"),
                            sql_type: "double precision",
                            // H-1: the accumulator is wider than the column.
                            from_raw: format!("sum({c}::double precision)"),
                            from_level: format!("sum({c}_sum)"),
                            strategy: *s,
                        });
                        out.push(AggColumn {
                            name: format!("{c}_count"),
                            sql_type: "bigint",
                            // H-2: a zero count is stored as NULL, not as 0.
                            from_raw: format!("nullif(count({c}), 0)"),
                            from_level: format!("nullif(sum({c}_count), 0)"),
                            strategy: *s,
                        });
                    }
                    MeanRepr::StatsAgg => out.push(AggColumn {
                        name: format!("{c}_stats"),
                        sql_type: "statssummary1",
                        from_raw: format!("stats_agg({c}::double precision)"),
                        from_level: format!("rollup({c}_stats)"),
                        strategy: *s,
                    }),
                },
                Strategy::Min => out.push(AggColumn {
                    name: format!("{c}_min"),
                    sql_type: ty,
                    from_raw: format!("min({c})"),
                    from_level: format!("min({c}_min)"),
                    strategy: *s,
                }),
                Strategy::Max => out.push(AggColumn {
                    name: format!("{c}_max"),
                    sql_type: ty,
                    from_raw: format!("max({c})"),
                    from_level: format!("max({c}_max)"),
                    strategy: *s,
                }),
                Strategy::Total => out.push(AggColumn {
                    name: format!("{c}_total"),
                    sql_type: "double precision",
                    // H-1 again: a total is a sum.
                    from_raw: format!("sum({c}::double precision)"),
                    from_level: format!("sum({c}_total)"),
                    strategy: *s,
                }),
                Strategy::CircularMean => {
                    // D-13: sine sum, cosine sum, count. Verified to compose through
                    // all three levels to within 3e-12 degrees. There is no column
                    // here that holds a finished angle.
                    out.push(AggColumn {
                        name: format!("{c}_sin_sum"),
                        sql_type: "double precision",
                        from_raw: format!("sum(sind({c}::double precision))"),
                        from_level: format!("sum({c}_sin_sum)"),
                        strategy: *s,
                    });
                    out.push(AggColumn {
                        name: format!("{c}_cos_sum"),
                        sql_type: "double precision",
                        from_raw: format!("sum(cosd({c}::double precision))"),
                        from_level: format!("sum({c}_cos_sum)"),
                        strategy: *s,
                    });
                    out.push(AggColumn {
                        name: format!("{c}_dir_count"),
                        sql_type: "bigint",
                        from_raw: format!("nullif(count({c}), 0)"),
                        from_level: format!("nullif(sum({c}_dir_count), 0)"),
                        strategy: *s,
                    });
                }
            }
        }
        out
    }
}

impl Registry {
    pub fn load(path: &Path) -> Result<Registry, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Metrics in protobuf field-number order — the canonical emission order, so that
    /// output does not depend on how the TOML happens to be arranged.
    pub fn metrics_ordered(&self) -> Vec<&Metric> {
        let mut v: Vec<&Metric> = self.metrics.iter().collect();
        v.sort_by_key(|m| m.field_number);
        v
    }

    pub fn live_metrics(&self) -> Vec<&Metric> {
        self.metrics_ordered()
            .into_iter()
            .filter(|m| !m.deprecated)
            .collect()
    }

    pub fn time_column(&self) -> &Dimension {
        self.dimensions
            .iter()
            .find(|d| d.role == Role::Time)
            .expect("validated: exactly one time dimension")
    }

    pub fn partition_column(&self) -> &Dimension {
        self.dimensions
            .iter()
            .find(|d| d.role == Role::Partition)
            .expect("validated: exactly one partition dimension")
    }

    pub fn level(&self, name: &str) -> Option<&Level> {
        self.levels.iter().find(|l| l.name == name)
    }

    /// Aggregate columns of one level, in canonical order.
    pub fn level_columns(&self, level: &str) -> Vec<(&Metric, AggColumn)> {
        let mut out = Vec::new();
        for m in self.metrics_ordered() {
            if m.in_level(&self.defaults, level) {
                for c in m.agg_columns(&self.defaults) {
                    out.push((m, c));
                }
            }
        }
        out
    }
}
