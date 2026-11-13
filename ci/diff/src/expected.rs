//! The expected-diff set, read from `expected.toml`, and the predicates it selects.
//!
//! A rule names a `class`, the scope it applies in, the keys or metrics it covers, and the
//! decision it comes from. The class is a predicate over the two values -- implemented here,
//! chosen there -- so a rule can fail on a key it names. That is the difference between
//! "`solarjoules` is allowed to differ" and "`solarjoules` on the new side is the Go side's
//! value multiplied by the bucket's reading count", and it is why this file has no `Ignore`
//! variant: there is nothing a rule can say that means "stop looking".

use std::collections::BTreeSet;
use std::fmt;

use serde::Deserialize;

use crate::numeric::{is_mean_times_count, within_float4_bound};
use crate::value::Val;

/// Where a difference was found. A rule declares the scopes it applies in, and a rule written
/// for `/span` cannot quietly start excusing a difference on `/latest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Mechanism (b).
    Readings,
    Span,
    Latest,
    Snow,
    Almanac,
}

impl Scope {
    pub fn token(self) -> &'static str {
        match self {
            Scope::Readings => "readings",
            Scope::Span => "span",
            Scope::Latest => "latest",
            Scope::Snow => "snow",
            Scope::Almanac => "almanac",
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

/// What the harness decided about one difference.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// The two sides agree. Not a difference at all.
    Equal,
    /// A difference that a rule predicted, named by the rule's decision id.
    Expected(String),
    /// A difference that no rule predicted. This is what makes a run red.
    Unexpected,
    /// A rule applies but its bound needs the bucket's reading count and none was supplied.
    ///
    /// Deliberately NOT clean. A tolerance whose n is unknown is not a tolerance, and a
    /// harness that fell back to a guess here would be reporting a bound it did not have.
    NoCensus(String),
}

impl Verdict {
    /// Whether this verdict lets a run stay green.
    pub fn is_clean(&self) -> bool {
        matches!(self, Verdict::Equal | Verdict::Expected(_))
    }
}

/// The Go side of one whole reading, for a predicate that is about a second column.
///
/// DEV-9's `windchill` case cannot be decided from two values: the prediction is that the Go
/// side stored *the air temperature* where the formula does not apply, so the rule has to be
/// able to ask what the air temperature was. A linear search over the column names is what
/// this does, and it costs nothing -- only the two rules that name a column ever call it.
pub struct RowView<'a> {
    pub columns: &'a [String],
    pub go: &'a [Val],
}

impl<'a> RowView<'a> {
    pub fn go_column(&self, name: &str) -> Option<&'a Val> {
        self.columns
            .iter()
            .position(|c| c == name)
            .map(|i| &self.go[i])
    }
}

/// One difference, offered to the rule set.
pub struct Candidate<'a> {
    pub scope: Scope,
    /// The JSON key, or the column name for mechanism (b).
    pub key: &'a str,
    pub go: &'a Val,
    pub rust: &'a Val,
    /// Readings in the bucket, from the census. `None` outside `/span`.
    pub n_readings: Option<u64>,
    /// The rest of the Go row, for a cross-column predicate. `None` outside (b).
    pub row: Option<&'a RowView<'a>>,
}

#[derive(Debug, Deserialize)]
struct RawFile {
    schema: u32,
    readings: RawReadings,
    responses: RawResponses,
}

#[derive(Debug, Deserialize)]
struct RawReadings {
    pair_tolerance_ms: i64,
    not_metrics: Vec<String>,
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
struct RawResponses {
    latest_pair_tolerance_ms: i64,
    not_values: Vec<String>,
    band: Vec<RawBand>,
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
struct RawBand {
    id: String,
    span: String,
    bucket_seconds: u64,
}

#[derive(Debug, Deserialize)]
struct RawRule {
    id: String,
    class: String,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    endpoints: Vec<String>,
    #[serde(default)]
    keys: Vec<String>,
    #[serde(default)]
    metrics: Vec<String>,
    #[serde(default)]
    max_buckets: Option<u32>,
    #[serde(default)]
    go_column: Option<String>,
    #[allow(dead_code)]
    because: String,
}

/// One `/span` aggregate level and the span string that selects it.
#[derive(Debug, Clone)]
pub struct Band {
    pub id: String,
    pub span: String,
    pub bucket_seconds: u64,
}

/// A rule's predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// The Go side wrote exactly 0.0 and the Rust side wrote nothing (D-26).
    NullVsZero,
    /// The Go side wrote the value of another column of the same reading and the Rust side
    /// wrote nothing (DEV-9, part 1).
    NullVsGoColumn,
    /// The Go side's `float4` accumulator bound (DEV-4).
    Float4Accumulated,
    /// The new side is the Go side times the bucket's reading count (DEV-7).
    MeanToTotal,
    /// The Go side's body is not JSON because it carries a non-finite literal, and the new
    /// side's parsed (DEV-11). A body-level rule, decided in `responses`.
    ///
    /// This is NOT a per-value predicate and the first draft of it was: "the Go side emitted
    /// `NaN` and the new side emitted 0" cannot fire, because `encoding/json` refuses a
    /// non-finite mid-stream and truncates the response -- so the Go body never parses and
    /// there is no pair of values to compare. The deviation is real and only mechanism (c)
    /// can ever see it; what it looks like is one side's body being unusable.
    NonFiniteBody,
    /// A coverage rule, decided in `responses`, not per value (DEV-12).
    ExtraLeadingBucket,
}

#[derive(Debug, Clone)]
struct Rule {
    id: String,
    class: Class,
    scopes: BTreeSet<Scope>,
    /// `None` means the rule declared `*`.
    keys: Option<BTreeSet<String>>,
    max_buckets: u32,
    /// The column `null_vs_go_column` compares the Go value against.
    go_column: Option<String>,
}

impl Rule {
    fn covers(&self, scope: Scope, key: &str) -> bool {
        if !self.scopes.contains(&scope) {
            return false;
        }
        match &self.keys {
            None => true,
            Some(set) => set.contains(key),
        }
    }
}

/// The whole expected-diff set.
#[derive(Debug, Clone)]
pub struct Expected {
    pub pair_tolerance_ms: i64,
    pub latest_pair_tolerance_ms: i64,
    pub not_metrics: BTreeSet<String>,
    pub not_values: BTreeSet<String>,
    pub bands: Vec<Band>,
    rules: Vec<Rule>,
}

impl Expected {
    /// Parse `expected.toml`.
    ///
    /// Every failure here is fatal and none of them is a warning. An expected-diff set that
    /// half-loaded would make a run green by losing the rule that would have failed it, or
    /// red by losing the rule that would have excused a predicted difference; neither is
    /// recoverable and both look like a result.
    pub fn parse(text: &str) -> Result<Expected, String> {
        let raw: RawFile = toml::from_str(text).map_err(|e| format!("expected.toml: {e}"))?;
        if raw.schema != 1 {
            return Err(format!(
                "expected.toml declares schema {}, and this harness reads schema 1",
                raw.schema
            ));
        }
        if raw.responses.band.is_empty() {
            return Err("expected.toml declares no [[responses.band]]".into());
        }
        let mut rules = Vec::new();
        for r in raw.readings.rule.iter() {
            rules.push(compile(r, Scope::Readings)?);
        }
        for r in raw.responses.rule.iter() {
            rules.push(compile(r, Scope::Span)?);
        }
        Ok(Expected {
            pair_tolerance_ms: raw.readings.pair_tolerance_ms,
            latest_pair_tolerance_ms: raw.responses.latest_pair_tolerance_ms,
            not_metrics: raw.readings.not_metrics.into_iter().collect(),
            not_values: raw.responses.not_values.into_iter().collect(),
            bands: raw
                .responses
                .band
                .into_iter()
                .map(|b| Band {
                    id: b.id,
                    span: b.span,
                    bucket_seconds: b.bucket_seconds,
                })
                .collect(),
            rules,
        })
    }

    /// The rule that predicts an unparseable Go body carrying a non-finite literal, if the
    /// set declares one for this scope.
    pub fn nonfinite_body_rule(&self, scope: Scope) -> Option<&str> {
        self.rules
            .iter()
            .find(|r| r.class == Class::NonFiniteBody && r.scopes.contains(&scope))
            .map(|r| r.id.as_str())
    }

    /// How many leading buckets may be present on the Rust side alone, per DEV-12.
    pub fn allowed_extra_leading_buckets(&self) -> (u32, Option<&str>) {
        for r in &self.rules {
            if r.class == Class::ExtraLeadingBucket {
                return (r.max_buckets, Some(&r.id));
            }
        }
        (0, None)
    }

    /// Decide one difference.
    pub fn classify(&self, c: &Candidate<'_>) -> Verdict {
        if c.go.same(c.rust) {
            return Verdict::Equal;
        }
        let mut deferred: Option<String> = None;
        for rule in &self.rules {
            if !rule.covers(c.scope, c.key) {
                continue;
            }
            match rule.class {
                Class::NullVsZero => {
                    if c.go.is_zero() && c.rust.is_null() {
                        return Verdict::Expected(rule.id.clone());
                    }
                }
                // Not a value predicate: `responses` consults `nonfinite_body_rule` when a
                // body fails to parse, which is before there are any values.
                Class::NonFiniteBody => {}
                Class::NullVsGoColumn => {
                    // The whole predicate: the new side is absent AND the Go side stored the
                    // named column's value here. A Go side that stored anything else does not
                    // match, which is what keeps this from being "windchill may differ".
                    if c.rust.is_null() {
                        if let (Some(row), Some(col)) = (c.row, rule.go_column.as_deref()) {
                            if let Some(reference) = row.go_column(col) {
                                if c.go.same(reference) && !c.go.is_null() {
                                    return Verdict::Expected(rule.id.clone());
                                }
                            }
                        }
                    }
                }
                Class::Float4Accumulated => match (c.go.num(), c.rust.num(), c.n_readings) {
                    (Some(a), Some(b), Some(n)) => {
                        if within_float4_bound(n, a, b) {
                            return Verdict::Expected(rule.id.clone());
                        }
                    }
                    // Both numbers and no census: the rule applies but its bound is unknown.
                    // Remembered rather than returned, so a second rule that can decide the
                    // difference outright still gets its turn.
                    (Some(_), Some(_), None) => deferred = Some(rule.id.clone()),
                    _ => {}
                },
                Class::MeanToTotal => match (c.go.num(), c.rust.num(), c.n_readings) {
                    (Some(mean), Some(total), Some(n)) => {
                        if is_mean_times_count(n, mean, total) {
                            return Verdict::Expected(rule.id.clone());
                        }
                    }
                    (Some(_), Some(_), None) => deferred = Some(rule.id.clone()),
                    _ => {}
                },
                // Not a value predicate. `responses` consults
                // `allowed_extra_leading_buckets` while it is pairing buckets, because a
                // bucket that exists on one side only has no pair to offer here.
                Class::ExtraLeadingBucket => {}
            }
        }
        match deferred {
            Some(id) => Verdict::NoCensus(id),
            None => Verdict::Unexpected,
        }
    }
}

fn compile(raw: &RawRule, default_scope: Scope) -> Result<Rule, String> {
    let class = match raw.class.as_str() {
        "null_vs_zero" => {
            // The direction is the whole predicate, so a rule that leaves it out is rejected
            // rather than defaulted: `go_zero_rust_null` is the intended change and the
            // reverse would be a defect being excused by the same words.
            match raw.direction.as_deref() {
                Some("go_zero_rust_null") => Class::NullVsZero,
                Some(other) => {
                    return Err(format!(
                        "rule {}: direction `{other}` is not implemented. The only direction \
                         D-26 predicts is `go_zero_rust_null`",
                        raw.id
                    ))
                }
                None => {
                    return Err(format!(
                        "rule {}: class null_vs_zero needs an explicit `direction`",
                        raw.id
                    ))
                }
            }
        }
        "null_vs_go_column" => {
            if raw.go_column.is_none() {
                return Err(format!(
                    "rule {}: class null_vs_go_column needs `go_column`, the column the Go \
                     side is predicted to have stored instead",
                    raw.id
                ));
            }
            Class::NullVsGoColumn
        }
        "float4_accumulated" => Class::Float4Accumulated,
        "mean_to_total" => Class::MeanToTotal,
        "nonfinite_body" => Class::NonFiniteBody,
        "extra_leading_bucket" => Class::ExtraLeadingBucket,
        other => {
            return Err(format!(
                "rule {}: class `{other}` is not implemented. A class is a predicate in \
                 src/expected.rs, so a new one is code and a review, not a line of data",
                raw.id
            ))
        }
    };

    let mut scopes = BTreeSet::new();
    if raw.endpoints.is_empty() {
        scopes.insert(default_scope);
    } else {
        for e in &raw.endpoints {
            let scope = match e.as_str() {
                "readings" => Scope::Readings,
                "span" => Scope::Span,
                "latest" => Scope::Latest,
                "snow" => Scope::Snow,
                "almanac" => Scope::Almanac,
                other => return Err(format!("rule {}: no such endpoint `{other}`", raw.id)),
            };
            scopes.insert(scope);
        }
    }

    // `keys` for a response rule and `metrics` for a reading rule are the same field with the
    // name its own half of the harness uses. Declaring both is a mistake worth refusing: the
    // reader would have to know which one won.
    let named: Vec<String> = match (raw.keys.is_empty(), raw.metrics.is_empty()) {
        (false, false) => {
            return Err(format!(
                "rule {}: declares both `keys` and `metrics`",
                raw.id
            ))
        }
        (false, true) => raw.keys.clone(),
        (true, false) => raw.metrics.clone(),
        (true, true) => Vec::new(),
    };
    let keys = if named.iter().any(|k| k == "*") {
        if named.len() != 1 {
            return Err(format!(
                "rule {}: `*` alongside named keys. One or the other, so that reading the \
                 rule does not require knowing whether `*` subsumes the list",
                raw.id
            ));
        }
        None
    } else if named.is_empty() {
        if matches!(class, Class::ExtraLeadingBucket | Class::NonFiniteBody) {
            None
        } else {
            return Err(format!(
                "rule {}: names no keys. A value rule that covers nothing is either a typo or \
                 a rule somebody meant to delete",
                raw.id
            ));
        }
    } else {
        Some(named.into_iter().collect())
    };

    if class == Class::ExtraLeadingBucket && raw.max_buckets.is_none() {
        return Err(format!(
            "rule {}: class extra_leading_bucket needs `max_buckets`",
            raw.id
        ));
    }

    Ok(Rule {
        id: raw.id.clone(),
        class,
        scopes,
        keys,
        max_buckets: raw.max_buckets.unwrap_or(0),
        go_column: raw.go_column.clone(),
    })
}
