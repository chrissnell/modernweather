//! Mechanism (b) -- same station, both daemons, diffed readings.
//!
//! Both daemons receive the same frames: a WeatherLink Live station broadcasts over UDP, and
//! the Campbell station broadcasts through the `ser2net` fan-out M5-1a measured. Each writes
//! to its own database. So the comparison is between two tables, keyed by the instant each
//! daemon stamped the frame it received, and the expected output is one class of difference
//! enumerated in advance: **NULL where Go writes `0.0` for a metric the station never
//! reported** (D-26), and nothing else.
//!
//! # Pairing, and why it is checked rather than assumed
//!
//! The two timestamps for one frame are not equal -- each is that process's receipt time, 3 ms
//! apart when measured on 2026-11-13. So readings are paired within a declared tolerance,
//! and a reading with two candidate partners inside it is reported as **ambiguous** rather
//! than resolved by taking the nearer. A harness that silently chose would mis-pair a window
//! in which one daemon dropped a frame, and every metric would then differ for a reason that
//! has nothing to do with either implementation.
//!
//! # Columns come from the two schemas, not from a list in here
//!
//! The 215 columns are `metrics/registry.toml`'s business and the generator's. Re-stating
//! them here would be a third copy to drift, so the harness reads both tables' column lists
//! at run time and reports the three sets it finds: compared, Go-only, Rust-only. A column
//! that exists on one side only is a schema difference and is reported as one -- which is a
//! different finding from a value difference and must not be averaged into it.

use std::collections::BTreeMap;

use crate::expected::{Candidate, Expected, RowView, Scope, Verdict};
use crate::psql::{quote_station, Psql};
use crate::source::RowSide;
use crate::value::Val;

/// Which table and identity column a side keeps its readings in.
#[derive(Debug, Clone, Copy)]
pub struct Shape {
    pub table: &'static str,
    pub ident: &'static str,
}

/// The Go daemon's hypertable.
pub const GO: Shape = Shape {
    table: "weather",
    ident: "stationname",
};
/// The new schema's (W4 D-5).
pub const RUST: Shape = Shape {
    table: "observations",
    ident: "station_id",
};

/// One reading: the instant, and one value per compared column.
#[derive(Debug, Clone)]
pub struct Reading {
    pub ts_us: i64,
    pub vals: Vec<Val>,
}

/// One side's readings over the window.
#[derive(Debug, Clone)]
pub struct Side {
    pub columns: Vec<String>,
    pub rows: Vec<Reading>,
    /// Columns the table has that are not numeric and are therefore not metrics. Reported so
    /// that "not compared" is visible rather than silent.
    pub non_numeric: Vec<String>,
    pub provenance: String,
}

/// What (b) found, per metric.
#[derive(Debug, Default, Clone)]
pub struct MetricStat {
    pub compared: u64,
    pub equal: u64,
    pub expected: BTreeMap<String, u64>,
    pub unexpected: u64,
    /// The first unexpected difference, for the report's example.
    pub example: Option<(i64, String, String)>,
    /// Smallest and largest `rust - go` across the unexpected numeric differences.
    ///
    /// A constant offset is the signature of a calibration applied on one side and not the
    /// other, and reading 600 identical lines to notice that is work the harness can do.
    pub delta_min: Option<f64>,
    pub delta_max: Option<f64>,
    /// The same, reduced modulo 360.
    ///
    /// A wind vane's calibration wraps, so the raw delta for a constant offset of +94 is
    /// either +94 or -266 depending on the bearing and the plain range test never fires on
    /// the one column where this hint is worth the most. Reduced, it is +94 on every reading.
    pub delta_mod360_min: Option<f64>,
    pub delta_mod360_max: Option<f64>,
}

impl MetricStat {
    /// The constant offset modulo a turn, for a bearing. `None` unless every differing
    /// reading agrees, and unless the reduction says something the plain offset does not.
    pub fn constant_offset_mod360(&self) -> Option<f64> {
        if self.constant_offset().is_some() {
            return None;
        }
        match (self.delta_mod360_min, self.delta_mod360_max) {
            (Some(lo), Some(hi)) if lo.is_finite() && hi.is_finite() && (hi - lo).abs() <= 1e-6 => {
                Some(hi)
            }
            _ => None,
        }
    }

    /// The constant offset, if every unexpected difference is the same one.
    ///
    /// The finiteness check is not belt and braces: a difference where one side is NULL has
    /// no delta and is recorded as an infinite range, and `inf - -inf <= inf` is true -- so
    /// without it a column that is NULL throughout would be reported as a constant offset of
    /// infinity, which is both false and the opposite of the hint this is for.
    pub fn constant_offset(&self) -> Option<f64> {
        match (self.delta_min, self.delta_max) {
            (Some(lo), Some(hi))
                if lo.is_finite()
                    && hi.is_finite()
                    && (hi - lo).abs() <= 1e-6 * hi.abs().max(1.0) =>
            {
                Some(hi)
            }
            _ => None,
        }
    }
}

/// What (b) found.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub go: Side,
    pub rust: Side,
    /// Readings each side had inside the comparison window, which is what the report prints:
    /// the loaded counts include the pairing tolerance's slack and would not add up.
    pub go_in_window: u64,
    pub rust_in_window: u64,
    pub compared_columns: Vec<String>,
    pub go_only_columns: Vec<String>,
    pub rust_only_columns: Vec<String>,
    pub paired: u64,
    pub unpaired_go: u64,
    pub unpaired_rust: u64,
    pub ambiguous: u64,
    /// The largest pairing offset used, which says whether the tolerance was near its limit.
    pub max_offset_ms: i64,
    pub stats: BTreeMap<String, MetricStat>,
}

impl Outcome {
    /// The metrics carrying a difference no rule predicts.
    pub fn unexpected_metrics(&self) -> Vec<&String> {
        self.stats
            .iter()
            .filter(|(_, s)| s.unexpected > 0)
            .map(|(k, _)| k)
            .collect()
    }

    /// Whether the run meets M5-2's criterion for (b): only NULL-versus-zero, and nothing
    /// else -- including nothing unpaired and no schema difference, both of which mean the
    /// comparison did not cover what it claims to have covered.
    pub fn is_green(&self) -> bool {
        self.unexpected_metrics().is_empty()
            && self.ambiguous == 0
            && self.unpaired_go == 0
            && self.unpaired_rust == 0
            && self.go_only_columns.is_empty()
            && self.rust_only_columns.is_empty()
            && self.paired > 0
    }
}

/// The numeric SQL types a metric column can have. Anything else is not a metric and is
/// listed rather than compared.
fn is_numeric(data_type: &str) -> bool {
    matches!(
        data_type,
        "real" | "double precision" | "numeric" | "integer" | "bigint" | "smallint" | "boolean"
    )
}

/// Load one side.
///
/// `from_ms`/`until_ms` are the LOADING window and are wider than the comparison window by
/// the pairing tolerance: a frame whose two receipt stamps straddle the edge has to have both
/// halves available, or it becomes an unpaired reading and the run goes red for a
/// non-difference. [`diff`] is given the real window and restricts to it.
pub fn load(
    side: &RowSide,
    shape: Shape,
    side_label: &str,
    station: &str,
    from_ms: i64,
    until_ms: i64,
    expected: &Expected,
) -> Result<Side, String> {
    match side {
        RowSide::Tsv { path } => {
            let text =
                std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let mut lines = text.lines().filter(|l| !l.trim().is_empty());
            let header = lines
                .next()
                .ok_or_else(|| format!("{}: empty", path.display()))?;
            let cols: Vec<String> = header.split('\t').map(str::to_string).collect();
            if cols.first().map(String::as_str) != Some("time_us") {
                return Err(format!(
                    "{}: the first column must be `time_us`, which is what the harness's own \
                     query selects",
                    path.display()
                ));
            }
            // The same two filters the psql branch applies, because the documented way to
            // record a dump is to run the harness's own query -- so a dump carries the
            // identity columns too, and comparing `stationname` against `station_id` would
            // report the one thing held constant as a difference in every reading. The type
            // filter cannot be read from a TSV, so it is inferred below from the values.
            let columns: Vec<String> = cols[1..]
                .iter()
                .filter(|c| !expected.not_metrics.contains(*c))
                .cloned()
                .collect();
            let keep: Vec<usize> = cols[1..]
                .iter()
                .enumerate()
                .filter(|(_, c)| !expected.not_metrics.contains(*c))
                .map(|(i, _)| i + 1)
                .collect();
            let mut rows = Vec::new();
            for (i, line) in lines.enumerate() {
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() != cols.len() {
                    return Err(format!(
                        "{}:{}: {} fields against {} header columns",
                        path.display(),
                        i + 2,
                        f.len(),
                        cols.len()
                    ));
                }
                let ts_us: i64 = f[0].parse().map_err(|_| {
                    format!(
                        "{}:{}: `{}` is not a microsecond epoch",
                        path.display(),
                        i + 2,
                        f[0]
                    )
                })?;
                if ts_us / 1000 < from_ms || ts_us / 1000 >= until_ms {
                    continue;
                }
                rows.push(Reading {
                    ts_us,
                    vals: keep.iter().map(|i| Val::from_psql_field(f[*i])).collect(),
                });
            }
            rows.sort_by_key(|r| r.ts_us);
            // A column whose every non-NULL value fails to parse as a number is not a metric.
            // Inferred rather than declared, because a TSV carries no types -- and reported
            // rather than dropped silently, which is what the psql branch does with the same
            // class of column.
            let mut numeric = Vec::new();
            let mut non_numeric = Vec::new();
            for (i, col) in columns.iter().enumerate() {
                let mut saw_value = false;
                let mut saw_number = false;
                for row in &rows {
                    match &row.vals[i] {
                        Val::Null => {}
                        Val::Num { .. } => {
                            saw_value = true;
                            saw_number = true;
                        }
                        _ => saw_value = true,
                    }
                }
                if saw_value && !saw_number {
                    non_numeric.push(format!("{col} (no numeric value in the window)"));
                } else {
                    numeric.push(i);
                }
            }
            if !non_numeric.is_empty() {
                for row in rows.iter_mut() {
                    row.vals = numeric.iter().map(|i| row.vals[*i].clone()).collect();
                }
            }
            let columns: Vec<String> = numeric.iter().map(|i| columns[*i].clone()).collect();
            Ok(Side {
                columns,
                rows,
                non_numeric,
                provenance: format!("tsv:{}", path.display()),
            })
        }
        RowSide::Psql { command } => {
            // The label is which SIDE this is, not which table: an error that reads
            // "weather: no such table in the public schema of weather" names the same thing
            // twice and never says which of the two databases failed.
            let psql = Psql::new(&format!("{side_label} ({})", shape.table), command)?;
            let name = quote_station(station)?;
            let catalog = psql.rows(&format!(
                "SELECT column_name, data_type FROM information_schema.columns \
                 WHERE table_schema = 'public' AND table_name = '{}' ORDER BY ordinal_position",
                shape.table
            ))?;
            if catalog.is_empty() {
                return Err(format!(
                    "{}: no such table in the `public` schema of {}",
                    shape.table, psql.label
                ));
            }
            let mut columns = Vec::new();
            let mut non_numeric = Vec::new();
            for row in &catalog {
                let (col, ty) = (&row[0], row.get(1).map(String::as_str).unwrap_or(""));
                if expected.not_metrics.contains(col) {
                    continue;
                }
                if is_numeric(ty) {
                    columns.push(col.clone());
                } else {
                    non_numeric.push(format!("{col} ({ty})"));
                }
            }
            let select: Vec<String> = columns.iter().map(|c| format!("\"{c}\"::text")).collect();
            let sql = format!(
                "SELECT (extract(epoch from time) * 1000000)::bigint, {} FROM {} \
                 WHERE {} = {} AND time >= to_timestamp({}) AND time < to_timestamp({}) \
                 ORDER BY time",
                select.join(", "),
                shape.table,
                shape.ident,
                name,
                from_ms as f64 / 1000.0,
                until_ms as f64 / 1000.0,
            );
            let mut rows = Vec::new();
            for row in psql.rows(&sql)? {
                if row.len() != columns.len() + 1 {
                    return Err(format!(
                        "{}: {} fields for {} selected columns",
                        psql.label,
                        row.len(),
                        columns.len() + 1
                    ));
                }
                let ts_us: i64 = row[0]
                    .parse()
                    .map_err(|_| format!("{}: `{}` is not an epoch", psql.label, row[0]))?;
                rows.push(Reading {
                    ts_us,
                    vals: row[1..].iter().map(|v| Val::from_psql_field(v)).collect(),
                });
            }
            Ok(Side {
                columns,
                rows,
                non_numeric,
                provenance: psql.redacted(),
            })
        }
    }
}

/// Diff two already-loaded sides over `window`, in epoch milliseconds.
///
/// The sides are loaded wider than the window by the pairing tolerance and the window is
/// applied here: a Go reading inside it is in scope, and a Rust reading outside it is a
/// candidate partner but is never counted as unpaired. Without that, every run reported one
/// or two unpaired readings at each edge -- a frame received at 17:59:59.999 by one daemon and
/// at 18:00:00.002 by the other -- and `is_green` turned a non-difference red.
pub fn diff(go: Side, rust: Side, expected: &Expected, window: (i64, i64)) -> Outcome {
    let tolerance_us = expected.pair_tolerance_ms * 1_000;
    let (from_us, until_us) = (window.0 * 1_000, window.1 * 1_000);
    let in_window = |ts_us: i64| ts_us >= from_us && ts_us < until_us;

    let go_set: Vec<&String> = go.columns.iter().collect();
    let rust_set: Vec<&String> = rust.columns.iter().collect();
    let compared: Vec<String> = go_set
        .iter()
        .filter(|c| rust_set.contains(c))
        .map(|c| (*c).clone())
        .collect();
    let go_only: Vec<String> = go_set
        .iter()
        .filter(|c| !rust_set.contains(c))
        .map(|c| (*c).clone())
        .collect();
    let rust_only: Vec<String> = rust_set
        .iter()
        .filter(|c| !go_set.contains(c))
        .map(|c| (*c).clone())
        .collect();

    // Index into each side's value vector, once, rather than per reading per metric.
    let go_at: Vec<usize> = compared
        .iter()
        .map(|c| go.columns.iter().position(|g| g == c).unwrap())
        .collect();
    let rust_at: Vec<usize> = compared
        .iter()
        .map(|c| rust.columns.iter().position(|r| r == c).unwrap())
        .collect();

    let mut stats: BTreeMap<String, MetricStat> = compared
        .iter()
        .map(|c| (c.clone(), MetricStat::default()))
        .collect();

    let rust_times: Vec<i64> = rust.rows.iter().map(|r| r.ts_us).collect();
    let mut claimed = vec![false; rust.rows.len()];
    let (mut paired, mut unpaired_go, mut ambiguous, mut max_offset_us) = (0u64, 0u64, 0u64, 0i64);

    for g in &go.rows {
        if !in_window(g.ts_us) {
            continue;
        }
        let lo = rust_times.partition_point(|t| *t < g.ts_us - tolerance_us);
        let hi = rust_times.partition_point(|t| *t <= g.ts_us + tolerance_us);
        match hi - lo {
            0 => {
                unpaired_go += 1;
                continue;
            }
            1 => {}
            _ => {
                // Two candidates inside the tolerance. Not resolved here: see the module
                // documentation for why choosing would be worse than reporting.
                ambiguous += 1;
                continue;
            }
        }
        let j = lo;
        if claimed[j] {
            ambiguous += 1;
            continue;
        }
        claimed[j] = true;
        paired += 1;
        let r = &rust.rows[j];
        let offset = (r.ts_us - g.ts_us).abs();
        max_offset_us = max_offset_us.max(offset);

        for (k, metric) in compared.iter().enumerate() {
            let gv = &g.vals[go_at[k]];
            let rv = &r.vals[rust_at[k]];
            let stat = stats
                .get_mut(metric)
                .expect("every compared metric has a stat");
            stat.compared += 1;
            let verdict = expected.classify(&Candidate {
                scope: Scope::Readings,
                key: metric,
                go: gv,
                rust: rv,
                n_readings: None,
                // DEV-9's predicate is about a second column of the same reading -- "the Go
                // side stored the air temperature here" -- so the rule set is handed the
                // whole Go row rather than two values out of it.
                row: Some(&RowView {
                    columns: &go.columns,
                    go: &g.vals,
                }),
            });
            match verdict {
                Verdict::Equal => stat.equal += 1,
                Verdict::Expected(id) => *stat.expected.entry(id).or_default() += 1,
                Verdict::Unexpected | Verdict::NoCensus(_) => {
                    stat.unexpected += 1;
                    if stat.example.is_none() {
                        stat.example = Some((g.ts_us / 1_000, gv.to_string(), rv.to_string()));
                    }
                    if let (Some(a), Some(b)) = (gv.num(), rv.num()) {
                        let d = b - a;
                        stat.delta_min = Some(stat.delta_min.map_or(d, |m: f64| m.min(d)));
                        stat.delta_max = Some(stat.delta_max.map_or(d, |m: f64| m.max(d)));
                        let m = d.rem_euclid(360.0);
                        stat.delta_mod360_min =
                            Some(stat.delta_mod360_min.map_or(m, |v: f64| v.min(m)));
                        stat.delta_mod360_max =
                            Some(stat.delta_mod360_max.map_or(m, |v: f64| v.max(m)));
                    } else {
                        // A NULL on one side has no delta, and a "constant offset" claimed
                        // over a set that includes one would be false.
                        stat.delta_min = Some(f64::NEG_INFINITY);
                        stat.delta_max = Some(f64::INFINITY);
                        stat.delta_mod360_min = Some(f64::NEG_INFINITY);
                        stat.delta_mod360_max = Some(f64::INFINITY);
                    }
                }
            }
        }
    }

    // A Rust reading counts as unpaired only if the Go side has no candidate for it at all.
    // One whose partner exists but fell outside the comparison window is the straddling
    // frame again, seen from the other side: the pair is not compared, and reporting it as a
    // missing reading would be the same false red the loading slack exists to remove.
    let go_times: Vec<i64> = go.rows.iter().map(|r| r.ts_us).collect();
    let has_go_candidate = |ts_us: i64| {
        let lo = go_times.partition_point(|t| *t < ts_us - tolerance_us);
        let hi = go_times.partition_point(|t| *t <= ts_us + tolerance_us);
        hi > lo
    };
    let unpaired_rust = rust
        .rows
        .iter()
        .enumerate()
        .filter(|(j, r)| !claimed[*j] && in_window(r.ts_us) && !has_go_candidate(r.ts_us))
        .count() as u64;
    let go_in_window = go.rows.iter().filter(|r| in_window(r.ts_us)).count() as u64;
    let rust_in_window = rust.rows.iter().filter(|r| in_window(r.ts_us)).count() as u64;
    Outcome {
        go,
        rust,
        go_in_window,
        rust_in_window,
        compared_columns: compared,
        go_only_columns: go_only,
        rust_only_columns: rust_only,
        paired,
        unpaired_go,
        unpaired_rust,
        ambiguous,
        max_offset_ms: max_offset_us / 1_000,
        stats,
    }
}
