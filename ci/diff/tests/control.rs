//! The negative controls: the harness, made to differ on purpose.
//!
//! W1 **D-10**, which M5-2 restates as its own test strategy: *a differ that cannot be made
//! to differ proves nothing*. These are the half of that which needs no database and no
//! network, so they run on every pull request. The other half is in `ci/diff/self-test.sh`,
//! which drives the two binaries end to end over `recorded/` -- a clean run, then the same run
//! with a 1% error injected, asserting the first is green and the second names the metric.
//!
//! Every test here loads the **committed** `expected.toml` rather than a fixture of its own.
//! That is deliberate: a rule widened until it swallowed a real defect would still pass a
//! test written against a private copy, and the property worth asserting is about the file
//! that ships.

use mw_diff::expected::{Candidate, Expected, Scope, Verdict};
use mw_diff::inject::Injection;
use mw_diff::value::Val;

const COMMITTED: &str = include_str!("../expected.toml");

fn expected() -> Expected {
    Expected::parse(COMMITTED).expect("the committed expected.toml parses")
}

fn num(x: f64) -> Val {
    Val::Num {
        text: format!("{x}"),
        f: x,
    }
}

fn classify(scope: Scope, key: &str, go: &Val, rust: &Val, n: Option<u64>) -> Verdict {
    expected().classify(&Candidate {
        scope,
        key,
        go,
        rust,
        n_readings: n,
        row: None,
    })
}

// ---------------------------------------------------------------- the committed rule set

#[test]
fn the_committed_rule_set_declares_the_four_bands() {
    let e = expected();
    let ids: Vec<&str> = e.bands.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(
        ids,
        ["1m", "5m", "1h", "1d"],
        "legacy-api-contract.md 2.3's four levels"
    );
    // The span strings have to select the level they claim: a `/span/3h` that landed on the
    // 5m view would compare a band against itself under another name.
    let seconds = |s: &str| -> u64 { s.trim_end_matches('h').parse::<u64>().unwrap() * 3600 };
    assert!(seconds(&e.bands[0].span) <= 6 * 3600);
    assert!(seconds(&e.bands[1].span) > 6 * 3600 && seconds(&e.bands[1].span) <= 48 * 3600);
    assert!(seconds(&e.bands[2].span) > 48 * 3600 && seconds(&e.bands[2].span) <= 14 * 86400);
    assert!(seconds(&e.bands[3].span) > 14 * 86400);
}

// ---------------------------------------------------------------- DEV-4, and the 1% control

#[test]
fn dev4_absorbs_the_accumulators_last_bit_and_nothing_more() {
    // 2,160 readings in a daily bucket is DEV-4's own measured case.
    let inside = classify(
        Scope::Span,
        "period_rain",
        &num(1.0),
        &num(1.0 + 1.0e-7),
        Some(2_160),
    );
    assert_eq!(inside, Verdict::Expected("DEV-4".into()));
    let outside = classify(
        Scope::Span,
        "period_rain",
        &num(1.0),
        &num(1.01),
        Some(2_160),
    );
    assert_eq!(
        outside,
        Verdict::Unexpected,
        "a 1% error is not an accumulator's last bit"
    );
}

#[test]
fn a_one_percent_injection_is_outside_dev4_at_every_band() {
    // The control `self-test.sh` runs is 1%, and it has to be outside the bound at the 1d
    // band too -- where n is largest and the bound is widest. At production's five-second
    // cadence that is 17,280 readings in a daily bucket.
    for n in [12u64, 60, 720, 17_280, 43_200] {
        let v = classify(
            Scope::Span,
            "windd",
            &num(180.0),
            &num(180.0 * 1.01),
            Some(n),
        );
        assert_eq!(v, Verdict::Unexpected, "n = {n}");
    }
}

#[test]
fn dev4_without_a_census_is_not_clean() {
    let v = classify(
        Scope::Span,
        "period_rain",
        &num(1.0),
        &num(1.0 + 1.0e-7),
        None,
    );
    assert_eq!(v, Verdict::NoCensus("DEV-4".into()));
    assert!(
        !v.is_clean(),
        "a tolerance whose n is unknown is not a tolerance"
    );
}

#[test]
fn dev4_does_not_reach_an_endpoint_it_does_not_declare() {
    // `period_rain` is a /span key, and the rule names `endpoints = ["span"]`. The same key
    // on /latest is the station's own accumulator and has no bucket behind it.
    let v = classify(
        Scope::Latest,
        "period_rain",
        &num(1.0),
        &num(1.0 + 1.0e-7),
        Some(2_160),
    );
    assert_eq!(v, Verdict::Unexpected);
}

#[test]
fn dev4_does_not_reach_a_key_it_does_not_name() {
    let v = classify(
        Scope::Span,
        "otemp",
        &num(47.0),
        &num(47.000001),
        Some(2_160),
    );
    assert_eq!(
        v,
        Verdict::Unexpected,
        "the bound is for two statistics, not for every key"
    );
}

// ---------------------------------------------------------------- DEV-7

#[test]
fn dev7_asserts_the_relationship_and_not_the_key() {
    let mean = 0.0005859;
    // 30 readings in a 1m bucket at a two-second cadence.
    let total = mean * 30.0;
    assert_eq!(
        classify(
            Scope::Span,
            "solarjoules",
            &num(mean),
            &num(total),
            Some(30)
        ),
        Verdict::Expected("DEV-7".into())
    );
    // A side that multiplied by the wrong count is caught.
    assert_eq!(
        classify(
            Scope::Span,
            "solarjoules",
            &num(mean),
            &num(mean * 29.0),
            Some(30)
        ),
        Verdict::Unexpected
    );
    // And a total that is 1% wrong is still caught, which is the injected case.
    assert_eq!(
        classify(
            Scope::Span,
            "solarjoules",
            &num(mean),
            &num(total * 1.01),
            Some(30)
        ),
        Verdict::Unexpected
    );
}

#[test]
fn a_deviation_that_did_not_happen_is_a_count_and_not_a_verdict() {
    // Stated as a test because it is the one asymmetry in the design and it would otherwise
    // be discovered by somebody trusting a green run. If the new side left `solarjoules` as
    // the bucket MEAN -- DEV-7 reverted -- the two sides agree and the harness reports
    // `Equal`, not a failure. What surfaces it is the `expected` column: the endpoint's line
    // shows `DEV-7:0` where every previous run showed `DEV-7:126`.
    //
    // Making the rule mandatory instead was considered and rejected: `solarjoules` is zero
    // through every night, the mean and the total of zero are both zero, and a required rule
    // would turn every nocturnal daily bucket red.
    let mean = 0.0005859;
    assert_eq!(
        classify(Scope::Span, "solarjoules", &num(mean), &num(mean), Some(30)),
        Verdict::Equal
    );
}

// ---------------------------------------------------------------- D-26 and DEV-9

#[test]
fn d26_is_one_directional() {
    assert_eq!(
        classify(Scope::Readings, "extratemp2", &num(0.0), &Val::Null, None),
        Verdict::Expected("D-26".into())
    );
    // The reverse direction is a metric the new side invented and the Go side did not have.
    assert_eq!(
        classify(Scope::Readings, "extratemp2", &Val::Null, &num(0.0), None),
        Verdict::Unexpected
    );
    // And a real value against NULL is not the zero-fill class at all.
    assert_eq!(
        classify(Scope::Readings, "extratemp2", &num(1.2), &Val::Null, None),
        Verdict::Unexpected
    );
}

#[test]
fn d26_does_not_reach_a_response() {
    // The zero-fill is a column-level change. On the wire the frozen DTO zero-fills a NULL
    // anyway, so a /span key that went null is a defect in the read path, not D-26.
    assert_eq!(
        classify(Scope::Span, "extratemp2", &num(0.0), &Val::Null, Some(60)),
        Verdict::Unexpected
    );
}

#[test]
fn dev9_needs_the_go_side_to_have_stored_the_air_temperature() {
    use mw_diff::expected::RowView;
    let columns = vec!["outtemp".to_string(), "windchill".to_string()];
    let e = expected();
    let judge = |outtemp: f64, go_windchill: &Val, rust: &Val| -> Verdict {
        let vals = vec![num(outtemp), go_windchill.clone()];
        let row = RowView {
            columns: &columns,
            go: &vals,
        };
        e.classify(&Candidate {
            scope: Scope::Readings,
            key: "windchill",
            go: go_windchill,
            rust,
            n_readings: None,
            row: Some(&row),
        })
    };
    // The formula does not apply, so Go stored the air temperature and R1 stored NULL.
    assert_eq!(
        judge(47.7, &num(47.7), &Val::Null),
        Verdict::Expected("DEV-9".into())
    );
    // A Go wind chill that is NOT the air temperature is a real wind chill, and a NULL
    // against it is a defect the rule must not absorb.
    assert_eq!(judge(40.0, &num(33.6), &Val::Null), Verdict::Unexpected);
    // Two wind chills that disagree are a defect whatever the air temperature was.
    assert_eq!(judge(40.0, &num(33.6), &num(34.0)), Verdict::Unexpected);
}

#[test]
fn dev9_without_the_row_cannot_fire() {
    // Which is why mechanism (c) is handed no row: a response has no second column to check
    // the prediction against, so the rule declines rather than guessing.
    assert_eq!(
        classify(Scope::Readings, "windchill", &num(47.7), &Val::Null, None),
        Verdict::Unexpected
    );
}

// ---------------------------------------------------------------- DEV-11, DEV-12

#[test]
fn dev11_is_the_go_sides_non_finite_literal_against_a_zero() {
    assert_eq!(
        classify(
            Scope::Span,
            "otemp",
            &Val::NonFinite("NaN".into()),
            &num(0.0),
            Some(60)
        ),
        Verdict::Expected("DEV-11".into())
    );
    // Not the reverse, and not a non-finite against a number.
    assert_eq!(
        classify(
            Scope::Span,
            "otemp",
            &Val::NonFinite("NaN".into()),
            &num(47.0),
            Some(60)
        ),
        Verdict::Unexpected
    );
    assert_eq!(
        classify(
            Scope::Span,
            "otemp",
            &num(0.0),
            &Val::NonFinite("NaN".into()),
            Some(60)
        ),
        Verdict::Unexpected
    );
}

#[test]
fn dev12_allows_exactly_one_leading_bucket() {
    let e = expected();
    let (allowed, rule) = e.allowed_extra_leading_buckets();
    assert_eq!(allowed, 1);
    assert_eq!(rule, Some("DEV-12"));
}

// ---------------------------------------------------------------- the loader refuses

fn refuses(body: &str) -> String {
    let text = format!(
        "schema = 1\n\
         [readings]\n\
         pair_tolerance_ms = 1000\n\
         not_metrics = [\"time\"]\n\
         [responses]\n\
         latest_pair_tolerance_ms = 6000\n\
         not_values = [\"ts\"]\n\
         [[responses.band]]\n\
         id = \"1m\"\n\
         span = \"3h\"\n\
         bucket_seconds = 60\n\
         {body}\n"
    );
    Expected::parse(&text).expect_err("this rule set should be refused")
}

#[test]
fn an_unimplemented_class_is_refused_rather_than_ignored() {
    let e = refuses(
        "[[responses.rule]]\nid = \"X\"\nclass = \"whatever\"\nkeys = [\"a\"]\nbecause = \"\"",
    );
    assert!(e.contains("not implemented"), "{e}");
}

#[test]
fn null_vs_zero_without_a_direction_is_refused() {
    let e = refuses(
        "[[readings.rule]]\nid = \"X\"\nclass = \"null_vs_zero\"\nmetrics = [\"*\"]\nbecause = \"\"",
    );
    assert!(e.contains("direction"), "{e}");
}

#[test]
fn the_reverse_direction_is_refused_rather_than_implemented() {
    let e = refuses(
        "[[readings.rule]]\nid = \"X\"\nclass = \"null_vs_zero\"\ndirection = \"rust_zero_go_null\"\nmetrics = [\"*\"]\nbecause = \"\"",
    );
    assert!(e.contains("not implemented"), "{e}");
}

#[test]
fn a_wildcard_alongside_named_keys_is_refused() {
    let e = refuses(
        "[[responses.rule]]\nid = \"X\"\nclass = \"float4_accumulated\"\nkeys = [\"*\", \"windd\"]\nbecause = \"\"",
    );
    assert!(e.contains('*'), "{e}");
}

#[test]
fn a_value_rule_that_names_nothing_is_refused() {
    let e =
        refuses("[[responses.rule]]\nid = \"X\"\nclass = \"float4_accumulated\"\nbecause = \"\"");
    assert!(e.contains("names no keys"), "{e}");
}

#[test]
fn declaring_both_keys_and_metrics_is_refused() {
    let e = refuses(
        "[[responses.rule]]\nid = \"X\"\nclass = \"float4_accumulated\"\nkeys = [\"a\"]\nmetrics = [\"b\"]\nbecause = \"\"",
    );
    assert!(e.contains("both"), "{e}");
}

#[test]
fn extra_leading_bucket_without_a_count_is_refused() {
    let e =
        refuses("[[responses.rule]]\nid = \"X\"\nclass = \"extra_leading_bucket\"\nbecause = \"\"");
    assert!(e.contains("max_buckets"), "{e}");
}

#[test]
fn null_vs_go_column_without_a_column_is_refused() {
    let e = refuses(
        "[[readings.rule]]\nid = \"X\"\nclass = \"null_vs_go_column\"\nmetrics = [\"windchill\"]\nbecause = \"\"",
    );
    assert!(e.contains("go_column"), "{e}");
}

#[test]
fn a_future_schema_is_refused() {
    let text = COMMITTED.replacen("schema = 1", "schema = 2", 1);
    let e = Expected::parse(&text).expect_err("a schema this harness does not read");
    assert!(e.contains("schema 2"), "{e}");
}

// ---------------------------------------------------------------- the injection itself

#[test]
fn an_injection_reaches_every_element_of_a_span_array() {
    let mut body: serde_json::Value = serde_json::from_str(
        r#"[{"ts":1,"period_rain":1.0},{"ts":2,"period_rain":2.0},{"ts":3,"period_rain":0.0}]"#,
    )
    .unwrap();
    let moved = Injection::parse("period_rain:1:rust")
        .unwrap()
        .apply_json(&mut body);
    let values: Vec<f64> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["period_rain"].as_f64().unwrap())
        .collect();
    assert_eq!(
        values,
        vec![1.01, 2.02, 0.0],
        "every element, and zero scales to itself"
    );
    assert_eq!(
        moved, 2,
        "the count is what makes an injection that moved nothing visible"
    );
    // The pairing key is untouched: an injection that moved `ts` would unpair the buckets
    // and the harness would report a coverage difference instead of a value one.
    assert_eq!(body[0]["ts"].as_i64(), Some(1));
}

#[test]
fn a_zero_percent_injection_is_refused() {
    let e = Injection::parse("period_rain:0").expect_err("not a control");
    assert!(e.contains("not a control"), "{e}");
}

#[test]
fn an_injection_names_its_side_in_the_report() {
    assert!(Injection::parse("otemp:1:go")
        .unwrap()
        .describe()
        .contains("Go side"));
    assert!(Injection::parse("otemp:1")
        .unwrap()
        .describe()
        .contains("Rust side"));
}
