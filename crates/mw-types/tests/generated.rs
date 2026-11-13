//! `Reading`'s three generated accessors, against the registry, one metric at a time.
//!
//! **Why this exists, and why now.** `Reading::value` has had a caller since M3-3 — the
//! writer binds every column from it — and `Reading::get` has had one since M2-1. Neither
//! had a test that walked `METRICS`: the writer's own tests assert the *statement* it
//! builds and a handful of round trips through a real database, so a single arm pairing the
//! wrong field would have shown up as one column that is always NULL for one metric, with
//! nothing anywhere naming it.
//!
//! `Reading::set` is implementation-plan.md **M4-1**'s addition and is what forced the
//! question. `ReadingStore::latest` decodes 214 columns and places each one through it, so
//! it is `value`'s inverse over the same generated list — and an inverse pair is exactly the
//! kind of thing that is worth asserting *as a pair*, metric by metric, rather than hoping
//! that one generator loop cannot disagree with the one above it.
//!
//! **An integration test rather than a unit one**, on purpose: `mw_types::generated` is
//! private to the crate (W3 §1.6, rule 1), so a test outside it can only reach what the
//! crate re-exports. That is the same surface every other crate has, which makes this a
//! test of the contract rather than of the generator's internals.
//!
//! It also runs under `--cfg loom` — there is no runtime here and nothing to interleave.

use chrono::{DateTime, Utc};
use mw_types::{MetricValue, Reading, SqlType, METRICS};

/// A distinct value per metric in the type its column holds.
///
/// Distinct, so that a `set` arm writing the *neighbouring* field is caught: if every
/// metric were set to the same number, swapping two `float4` fields would round-trip
/// perfectly. The index is folded in, which makes each value unique within its type.
fn probe(sql_type: SqlType, i: usize) -> MetricValue<'static> {
    match sql_type {
        // `i` as an exact f32: every index in this registry is far inside the mantissa, and
        // the fractional part is there so that a field declared `int4` cannot hold it.
        SqlType::Float4 => MetricValue::Float(i as f32 + 0.5),
        SqlType::Int4 => MetricValue::Int(1_000 + i as i32),
        // Past 2^32, so a column narrowed to `int4` would not hold it.
        SqlType::Int8 => MetricValue::BigInt(4_294_967_296 + i as i64),
        // A leaked `&'static str` per metric: the registry is 212 entries and this is one
        // test process, so the alternative -- threading a lifetime through the probe table
        // -- buys nothing a reader would thank us for.
        SqlType::Text => MetricValue::Text(Box::leak(format!("probe-{i}").into_boxed_str())),
        SqlType::Timestamptz => MetricValue::Time(
            DateTime::<Utc>::from_timestamp(1_767_312_000 + i as i64, 0)
                .expect("a constant that is an instant"),
        ),
    }
}

/// A variant of a **different** type from `sql_type`, for the rejection half.
fn wrong(sql_type: SqlType) -> MetricValue<'static> {
    match sql_type {
        SqlType::Float4 => MetricValue::Int(1),
        _ => MetricValue::Float(1.0),
    }
}

/// **`set` is `value`'s inverse, for every metric the registry declares.**
///
/// One `Reading` per metric rather than one for all of them, so that "the field I read back
/// is the field I wrote" is a statement about *that* metric and not about the accumulated
/// state of 212 writes.
#[test]
fn every_metric_round_trips_through_set_and_value() {
    assert!(
        METRICS.len() > 200,
        "the registry should have the full metric set; got {}",
        METRICS.len()
    );
    for (i, meta) in METRICS.iter().enumerate() {
        let written = probe(meta.sql_type, i);
        let mut reading = Reading::default();

        assert!(
            reading.value(meta.key).is_none(),
            "{}: a default Reading reports nothing, which is D-26's premise",
            meta.column
        );
        assert!(
            reading.set(meta.key, written),
            "{}: `set` refused a {} for a {} column",
            meta.column,
            variant(written),
            meta.sql_type.ddl()
        );
        assert_eq!(
            reading.value(meta.key),
            Some(written),
            "{}: `set` and `value` disagree about which field this metric is",
            meta.column
        );
    }
}

/// **Exactly one field moves per `set`.** The assertion the test above cannot make: a
/// round trip through one accessor pair is still green if the arm writes the right field
/// *and* a second one.
#[test]
fn setting_one_metric_leaves_every_other_absent() {
    for (i, meta) in METRICS.iter().enumerate() {
        let mut reading = Reading::default();
        assert!(reading.set(meta.key, probe(meta.sql_type, i)));
        let also_set: Vec<&str> = METRICS
            .iter()
            .filter(|other| other.key != meta.key && reading.value(other.key).is_some())
            .map(|other| other.column)
            .collect();
        assert!(
            also_set.is_empty(),
            "setting `{}` also wrote {also_set:?}",
            meta.column
        );
    }
}

/// A value of the wrong variant is **refused and writes nothing** — which is what keeps a
/// decode bug a visible `false` rather than a metric that silently reads back NULL for one
/// SQL type. `mw_store::reader::latest` turns that `false` into a named `StoreError`.
#[test]
fn a_value_of_the_wrong_type_is_refused_and_changes_nothing() {
    for meta in METRICS {
        let mut reading = Reading::default();
        assert!(
            !reading.set(meta.key, wrong(meta.sql_type)),
            "{}: `set` accepted a {} for a {} column",
            meta.column,
            variant(wrong(meta.sql_type)),
            meta.sql_type.ddl()
        );
        assert!(
            reading.value(meta.key).is_none(),
            "{}: a refused `set` still wrote something",
            meta.column
        );
    }
}

/// `get` is the numeric view of the same field, and it answers `None` for the fifteen
/// metrics that are not numbers — which is the reason `value` exists beside it, and the
/// reason a writer driven off `get` would store a text metric as NULL.
#[test]
fn get_agrees_with_value_where_a_metric_is_numeric() {
    let mut numeric = 0usize;
    let mut not = 0usize;
    for (i, meta) in METRICS.iter().enumerate() {
        let mut reading = Reading::default();
        assert!(reading.set(meta.key, probe(meta.sql_type, i)));
        match probe(meta.sql_type, i) {
            MetricValue::Float(v) => {
                numeric += 1;
                assert_eq!(reading.get(meta.key), Some(f64::from(v)), "{}", meta.column);
            }
            MetricValue::Int(v) => {
                numeric += 1;
                assert_eq!(reading.get(meta.key), Some(f64::from(v)), "{}", meta.column);
            }
            MetricValue::BigInt(v) => {
                numeric += 1;
                // Past 2^32 and inside 2^53, so the cast is exact and the assertion is
                // about the field rather than about float precision.
                assert_eq!(reading.get(meta.key), Some(v as f64), "{}", meta.column);
            }
            MetricValue::Text(_) | MetricValue::Time(_) => {
                not += 1;
                assert_eq!(
                    reading.get(meta.key),
                    None,
                    "{}: `get` must not invent a number for a non-numeric metric",
                    meta.column
                );
            }
        }
    }
    assert_eq!(
        not, 15,
        "the registry's eleven text and four timestamptz metrics, which is the figure \
         `MetricValue`'s own documentation quotes"
    );
    assert_eq!(numeric + not, METRICS.len());
}

/// The variant's name, for a failure message.
fn variant(v: MetricValue<'_>) -> &'static str {
    match v {
        MetricValue::Float(_) => "float4",
        MetricValue::Int(_) => "int4",
        MetricValue::BigInt(_) => "int8",
        MetricValue::Text(_) => "text",
        MetricValue::Time(_) => "timestamptz",
    }
}
