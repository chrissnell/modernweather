//! One scalar from either side, in a shape the predicates can decide over.
//!
//! The two sides arrive by different routes -- a JSON body over HTTP and a `psql` column --
//! and the comparison has to treat them identically. A column that is SQL NULL and a key
//! whose JSON value is `null` are the same fact about the reading, and the whole of mechanism
//! (b)'s expected output is one statement about that fact (D-26), so it cannot be a detail of
//! either reader.

use std::fmt;

/// A value from one side of the diff.
#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    /// SQL NULL, or JSON `null`, or an absent `omitempty` key. The three are one fact.
    Null,
    /// A finite number, with the literal the server or the database actually wrote.
    ///
    /// The text is kept because a difference in the last digit has to be printable as the two
    /// digits: formatting an `f64` back out would round both sides to the same string and the
    /// report would claim two equal numbers differ.
    Num {
        text: String,
        f: f64,
    },
    /// `NaN`, `+Inf` or `-Inf`. Not JSON, and the Go server emits all three (DEV-11).
    NonFinite(String),
    Str(String),
    Bool(bool),
    /// An array or an object, compared by its serialisation. No frozen key holds one, so this
    /// exists to make an unexpected shape a reported difference rather than a panic.
    Composite(String),
}

impl Val {
    /// A number from its literal, classifying the non-finite spellings both sides can produce.
    pub fn from_number_literal(text: &str) -> Val {
        let t = text.trim();
        match t.parse::<f64>() {
            Ok(f) if f.is_finite() => Val::Num {
                text: t.to_string(),
                f,
            },
            // `"NaN".parse::<f64>()` succeeds, which is why the finite check is on the Ok arm
            // and not an `unwrap_or`. Everything that parses to a non-finite, and everything
            // that does not parse at all, is reported with the literal that produced it.
            _ => Val::NonFinite(t.to_string()),
        }
    }

    /// One `psql -tA` field. An empty field is NULL, which is why the harness runs psql with
    /// `-P null=\N`: without it an empty text column and a NULL would be the same field, and
    /// the whole of (b)'s expected class is about telling those apart.
    pub fn from_psql_field(field: &str) -> Val {
        if field == r"\N" {
            return Val::Null;
        }
        if field.is_empty() {
            // Reachable only if the caller dropped `-P null=\N`, so it is a hard error rather
            // than a guess between NULL and the empty string.
            return Val::Composite("<ambiguous empty field: psql is missing -P null=\\N>".into());
        }
        Val::from_number_literal(field)
    }

    /// One JSON value.
    pub fn from_json(v: &serde_json::Value) -> Val {
        match v {
            serde_json::Value::Null => Val::Null,
            serde_json::Value::Bool(b) => Val::Bool(*b),
            // With `arbitrary_precision` a number is carried as its literal, which is what
            // `to_string` returns here.
            serde_json::Value::Number(n) => Val::from_number_literal(&n.to_string()),
            serde_json::Value::String(s) => Val::Str(s.clone()),
            other => Val::Composite(other.to_string()),
        }
    }

    /// The finite number this is, if it is one.
    pub fn num(&self) -> Option<f64> {
        match self {
            Val::Num { f, .. } => Some(*f),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Val::Null)
    }

    /// Exactly zero, as the Go drivers write it for an unreported metric.
    pub fn is_zero(&self) -> bool {
        self.num() == Some(0.0)
    }

    /// Whether the two sides are the same value.
    ///
    /// Numbers compare by their `f64`, not by their literal: a side that wrote `0` where the
    /// other wrote `0.0` has not produced a different number, and numeric *formatting* is
    /// mechanism (a)'s job -- the conformance suite asserts it against the frozen fixtures and
    /// would catch a change here that this harness is deliberately blind to.
    pub fn same(&self, other: &Val) -> bool {
        match (self, other) {
            (Val::Num { f: a, .. }, Val::Num { f: b, .. }) => a == b,
            (a, b) => a == b,
        }
    }
}

impl fmt::Display for Val {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Val::Null => f.write_str("NULL"),
            Val::Num { text, .. } => f.write_str(text),
            Val::NonFinite(t) => f.write_str(t),
            Val::Str(s) => write!(f, "{s:?}"),
            Val::Bool(b) => write!(f, "{b}"),
            Val::Composite(s) => f.write_str(s),
        }
    }
}
