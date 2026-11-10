//! The escapers, asserted against output produced by **running** Go's `html/template`.
//!
//! `tests/golden/go-escapes.tsv` is produced by `cargo xtask go-golden` from
//! `ci/go-golden/escapes`, which renders
//! a one-action template in each of four contexts over every ASCII codepoint and a list of
//! adversarial strings, and prints the interpolation's bytes. That is the same way M4-2
//! pinned the enrich formulae -- against values the Go code produced rather than against a
//! reading of it -- and it is the only way a claim like "this reproduces `html/template`"
//! can be checked instead of believed.
//!
//! The table carries five context tags:
//!
//! | Tag | Go function | What it covers in the page |
//! |---|---|---|
//! | `text` | `htmlEscaper` | `<h1>{{ .StationName }}</h1>` |
//! | `attr` | `attrEscaper` | `content="app-id={{ .AppleAppID }}"` |
//! | `title` | `rcdataEscaper` | `<title>{{ .StationName }}</title>` |
//! | `jsstr` | `jsStrEscaper` | `"token": "{{ .RadarToken }}"` inside the JSON block |
//! | `jsval-f64`, `jsval-i64` | `jsValEscaper` | `"lat": {{ .Latitude }}` in the same block |
//!
//! **The first three must agree with each other**, which is the claim §7.1 makes implicitly
//! by naming one escaper, and which [`one_escaper_covers_all_three_html_contexts`] turns
//! into a measurement rather than an assumption.

use std::collections::BTreeMap;
use std::path::PathBuf;

use mw_web::escape::escape;
use mw_web::gojs::{js_integer, js_number, js_string};

/// One row: a context tag, the input and the output Go produced.
struct Row {
    context: String,
    input: String,
    output: String,
}

fn golden() -> Vec<Row> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("go-escapes.tsv");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let rows: Vec<Row> = text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|line| {
            let mut f = line.split('\t');
            let context = f.next().expect("a context tag").to_string();
            let input = f.next().expect("an input").to_string();
            let output = f.next().expect("an output").to_string();
            Row {
                context,
                input,
                output,
            }
        })
        .collect();
    assert!(
        rows.len() > 500,
        "the golden table is {} rows; it should carry every ASCII codepoint in four \
         contexts plus the adversarial set",
        rows.len()
    );
    rows
}

/// A hex field back to the bytes it encodes, then to a `String`.
///
/// Every input and output in the table is valid UTF-8: the generator interpolates Go
/// strings, and `html/template` never produces an invalid sequence from one.
fn unhex(hex: &str) -> String {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("a hex pair"))
        .collect();
    String::from_utf8(bytes).expect("the golden table is UTF-8")
}

/// **The HTML escaper, byte for byte against Go, over every row in all three HTML
/// contexts.**
#[test]
fn the_html_escaper_is_gos_html_template() {
    let mut checked = 0;
    for row in golden() {
        if !matches!(row.context.as_str(), "text" | "attr" | "title") {
            continue;
        }
        let input = unhex(&row.input);
        let expected = unhex(&row.output);
        assert_eq!(
            escape(&input),
            expected,
            "{}: {input:?} -- Go writes {expected:?}",
            row.context
        );
        checked += 1;
    }
    assert!(checked >= 3 * 128, "only {checked} HTML rows were checked");
}

/// §7.1 names **one** escaper for the page's HTML interpolations. That is only right if
/// `htmlEscaper`, `attrEscaper` and `rcdataEscaper` agree for a plain string -- so the claim
/// is measured here rather than assumed.
#[test]
fn one_escaper_covers_all_three_html_contexts() {
    let mut by_input: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for row in golden() {
        if !matches!(row.context.as_str(), "text" | "attr" | "title") {
            continue;
        }
        by_input
            .entry(row.input)
            .or_default()
            .insert(row.context, row.output);
    }
    assert!(!by_input.is_empty());
    for (input, outputs) in &by_input {
        assert_eq!(outputs.len(), 3, "{input}: not every context was captured");
        let distinct: std::collections::BTreeSet<&String> = outputs.values().collect();
        assert_eq!(
            distinct.len(),
            1,
            "{:?} escapes differently per context: {outputs:?}",
            unhex(input)
        );
    }
}

/// **`+` is in Go's table and not in §7.1's prose.** Called out as its own test because it
/// is the one entry a careful reading of the design would have left out, and because every
/// value an operator can type is a candidate: a station name, a radar token, an Apple app
/// id.
#[test]
fn the_plus_entry_the_prose_omits() {
    let row = golden()
        .into_iter()
        .find(|r| r.context == "text" && unhex(&r.input) == "a+b")
        .expect("the adversarial set carries `a+b`");
    assert_eq!(unhex(&row.output), "a&#43;b");
    assert_eq!(escape("a+b"), "a&#43;b");
}

/// And NUL, which §7.1 writes as `&#0;` and Go writes as `U+FFFD`.
#[test]
fn the_nul_replacement_is_the_replacement_character() {
    let row = golden()
        .into_iter()
        .find(|r| r.context == "text" && unhex(&r.input) == "a\0z")
        .expect("the ASCII sweep covers NUL");
    assert_eq!(unhex(&row.output), "a\u{FFFD}z");
    assert_eq!(escape("a\0z"), "a\u{FFFD}z");
}

/// The JavaScript string escaper, over the same rows.
#[test]
fn the_js_string_escaper_is_gos_js_template() {
    let mut checked = 0;
    for row in golden() {
        if row.context != "jsstr" {
            continue;
        }
        let input = unhex(&row.input);
        let expected = unhex(&row.output);
        assert_eq!(
            js_string(&input),
            expected,
            "jsstr: {input:?} -- Go writes {expected:?}"
        );
        checked += 1;
    }
    assert!(checked >= 128, "only {checked} JS-string rows were checked");
}

/// `jsValEscaper`, including the padding and Go's exponent spelling.
#[test]
fn the_js_value_escaper_is_gos_js_template() {
    let mut floats = 0;
    let mut integers = 0;
    for row in golden() {
        let expected = unhex(&row.output);
        match row.context.as_str() {
            "jsval-f64" => {
                let bits = u64::from_str_radix(&row.input, 16).expect("a 64-bit pattern");
                let value = f64::from_bits(bits);
                assert_eq!(
                    js_number(value).as_str(),
                    expected,
                    "jsval-f64: {value} -- Go writes {expected:?}"
                );
                floats += 1;
            }
            "jsval-i64" => {
                let value: i64 = row.input.parse().expect("an integer");
                assert_eq!(
                    js_integer(value).as_str(),
                    expected,
                    "jsval-i64: {value} -- Go writes {expected:?}"
                );
                integers += 1;
            }
            _ => continue,
        }
    }
    assert!(floats >= 15, "only {floats} float rows were checked");
    assert!(integers >= 5, "only {integers} integer rows were checked");
}

/// The composition the template relies on: askama's escaper runs over a JS-escaped value,
/// and must be a no-op. Over **every** row in the table, not over a curated few.
#[test]
fn the_html_escaper_is_a_no_op_over_every_js_escaped_value() {
    for row in golden() {
        if row.context != "jsstr" {
            continue;
        }
        let escaped = unhex(&row.output);
        assert_eq!(
            escape(&escaped),
            escaped,
            "the page's escaper changed a JS string: {escaped:?}"
        );
    }
}

/// **The one token the page's escaper would mangle.** Go writes `1e+21` for a magnitude
/// outside `[1e-6, 1e21)`, and `&#43;` there is not JSON -- so the two coordinate fields are
/// the crate's only escaping bypass.
///
/// This asserts the *hazard* and nothing more. The test that asserts the page is right is
/// `byte_diff.rs`'s `an_exponent_coordinate_renders_as_gos_bytes`, which renders one: M4-3's
/// code review found that this file's earlier version asserted only the hazard, so it passed
/// while the page shipped the entity.
#[test]
fn an_exponent_is_the_one_token_the_html_escaper_would_mangle() {
    let exponents: Vec<String> = golden()
        .into_iter()
        .filter(|r| r.context == "jsval-f64")
        .map(|r| unhex(&r.output))
        .filter(|token| token.contains("e+") || token.contains("e-"))
        .collect();
    assert!(
        !exponents.is_empty(),
        "the golden table must carry at least one exponent form, or this test says nothing"
    );
    let mut plus = 0;
    for token in &exponents {
        if token.contains("e+") {
            assert_ne!(
                escape(token),
                *token,
                "{token:?} is why the coordinates bypass the escaper"
            );
            plus += 1;
        } else {
            // A negative exponent has no `+`, so the escaper is already a no-op over it.
            assert_eq!(escape(token), *token, "{token:?}");
        }
    }
    assert!(plus > 0, "no `e+` token in the table");
}
