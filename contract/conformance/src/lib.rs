//! Replays the golden fixtures in `contract/fixtures/` against a live HTTP base URL and
//! asserts that the responses are compatible with the frozen legacy contract.
//!
//! "Compatible" deliberately does not mean "identical bytes": the values change every two
//! seconds. It means the properties a client can actually break on:
//!
//! * status code
//! * the headers the fixture recorded (with `max-age=<n>` compared as a pattern)
//! * for JSON: the exact key set at every level, the JSON type of every value, and —
//!   where the encoder is deterministic — key order
//! * numeric formatting, checked against a *convention* rather than against the fixture's
//!   literal. Go's `encoding/json` writes the shortest representation that round-trips, so
//!   a `float32` field emits `65` when its value is 65 and `65.3` when it is 65.3: the
//!   integer-or-decimal question is a property of the VALUE, not of the field, and
//!   comparing it field-by-field against a fixture would fail on live data. What is
//!   structural, and what this asserts, is that a decimal point appears if and only if the
//!   value has a fractional part. `/forecast` is exempt: its `data` is Postgres jsonb
//!   written back verbatim, so the literal is whatever was stored (`0.0`, not `0`).
//! * whether the body is newline-terminated
//! * for plain-text error bodies: the exact bytes
//! * for HTML and script bodies: the substrings a client or template consumer depends on

use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// Ordered, so key order is checkable; the order is only *asserted* when the fixture says to.
    Object(Vec<(String, Shape)>),
    /// The distinct element shapes, in first-seen order. Heterogeneous arrays are real:
    /// `/api/stations` emits the nested `website` object only for stations bound to one.
    Array(Vec<Shape>),
    Number,
    String,
    Bool,
    Null,
}

impl Shape {
    pub fn of(v: &Value) -> Result<Shape, String> {
        Ok(match v {
            Value::Object(m) => Shape::Object(
                m.iter()
                    .map(|(k, v)| Shape::of(v).map(|s| (k.clone(), s)))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Array(a) => {
                let mut variants: Vec<Shape> = Vec::new();
                for el in a {
                    let s = Shape::of(el)?;
                    if !variants.contains(&s) {
                        variants.push(s);
                    }
                }
                Shape::Array(variants)
            }
            Value::Number(_) => Shape::Number,
            Value::String(_) => Shape::String,
            Value::Bool(_) => Shape::Bool,
            Value::Null => Shape::Null,
        })
    }

    fn kind(&self) -> &'static str {
        match self {
            Shape::Object(_) => "object",
            Shape::Array(_) => "array",
            Shape::Number => "number",
            Shape::String => "string",
            Shape::Bool => "boolean",
            Shape::Null => "null",
        }
    }
}

/// Compare two shapes. `order` controls whether object key *order* is a failure or merely
/// a difference to ignore; key *membership* and value shapes always matter.
pub fn compare(expected: &Shape, actual: &Shape, order: bool) -> Vec<String> {
    let mut out = Vec::new();
    walk(expected, actual, "$", order, &mut out);
    out
}

fn walk(e: &Shape, a: &Shape, path: &str, order: bool, out: &mut Vec<String>) {
    match (e, a) {
        (Shape::Object(ef), Shape::Object(af)) => {
            let em: BTreeMap<_, _> = ef.iter().map(|(k, v)| (k.as_str(), v)).collect();
            let am: BTreeMap<_, _> = af.iter().map(|(k, v)| (k.as_str(), v)).collect();
            for k in em.keys() {
                if !am.contains_key(k) {
                    out.push(format!("{path}.{k}: key MISSING from the response"));
                }
            }
            for k in am.keys() {
                if !em.contains_key(k) {
                    out.push(format!("{path}.{k}: UNEXPECTED key not present in the fixture"));
                }
            }
            for (k, ev) in &em {
                if let Some(av) = am.get(k) {
                    walk(ev, av, &format!("{path}.{k}"), order, out);
                }
            }
            if order {
                let ek: Vec<_> = ef.iter().map(|(k, _)| k.as_str()).collect();
                let ak: Vec<_> = af.iter().map(|(k, _)| k.as_str()).collect();
                if ek.len() == ak.len() && ek != ak {
                    let first = ek.iter().zip(&ak).position(|(x, y)| x != y).unwrap_or(0);
                    out.push(format!(
                        "{path}: key ORDER differs, first at index {first}: expected {:?}, got {:?}",
                        ek[first], ak[first]
                    ));
                }
            }
        }
        (Shape::Array(ev), Shape::Array(av)) => {
            // Every element shape the response produced must match one the fixture recorded.
            // The converse is not required: a fixture may have captured a variant that this
            // run's data does not happen to contain.
            for (i, a) in av.iter().enumerate() {
                if ev.contains(a) {
                    continue;
                }
                let best = ev
                    .iter()
                    .map(|e| (diff(e, a, &format!("{path}[]")), e))
                    .min_by_key(|(d, _)| d.lines().count());
                match best {
                    None => out.push(format!(
                        "{path}[]: the response has elements but the fixture recorded an empty array"
                    )),
                    Some((d, _)) => out.push(format!(
                        "{path}[]: element shape variant {i} matches no shape in the fixture; \
                         closest difference:\n{d}"
                    )),
                }
            }
        }
        _ if std::mem::discriminant(e) == std::mem::discriminant(a) => {}
        _ => out.push(format!("{path}: TYPE differs: expected {}, got {}", e.kind(), a.kind())),
    }
}

fn diff(a: &Shape, b: &Shape, path: &str) -> String {
    let mut v = Vec::new();
    walk(a, b, path, false, &mut v);
    v.join("\n")
}

/// Assert that every numeric literal in `v` follows Go's `encoding/json` convention: the
/// shortest decimal that round-trips, which means a decimal point appears if and only if the
/// value has a fractional part, and exponent notation only outside [1e-6, 1e21).
pub fn check_number_literals(v: &Value, path: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                check_number_literals(x, &format!("{path}.{k}"), out);
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                check_number_literals(x, &format!("{path}[{i}]"), out);
            }
        }
        Value::Number(n) => {
            // `arbitrary_precision` keeps the literal exactly as the server wrote it.
            let lit = n.as_str();
            let Ok(f) = lit.parse::<f64>() else {
                out.push(format!("{path}: numeric literal {lit:?} does not parse as a number"));
                return;
            };
            let abs = f.abs();
            let exponential = lit.contains('e') || lit.contains('E');
            if exponential {
                if abs != 0.0 && abs >= 1e-6 && abs < 1e21 {
                    out.push(format!(
                        "{path}: numeric literal {lit:?} uses exponent notation, but Go's encoder \
                         only does that outside [1e-6, 1e21)"
                    ));
                }
                return;
            }
            let has_point = lit.contains('.');
            let integral = f.fract() == 0.0;
            if integral && has_point {
                out.push(format!(
                    "{path}: numeric literal {lit:?} has a decimal point for an integral value; \
                     Go's encoder writes {:.0}", f
                ));
            } else if !integral && !has_point {
                out.push(format!(
                    "{path}: numeric literal {lit:?} has no decimal point for a fractional value"
                ));
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------

pub struct Fixture {
    pub name: String,
    pub description: String,
    pub json: Value,
}

impl Fixture {
    fn b(&self, path: &[&str], default: bool) -> bool {
        let mut v = &self.json;
        for p in path {
            match v.get(p) {
                Some(x) => v = x,
                None => return default,
            }
        }
        v.as_bool().unwrap_or(default)
    }
}

pub fn load_fixtures(dir: &Path) -> std::io::Result<Vec<Fixture>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        let text = std::fs::read_to_string(&p)?;
        let json: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{}: not valid JSON: {e}", p.display()));
        out.push(Fixture {
            name: json["name"].as_str().unwrap_or("<unnamed>").to_string(),
            description: json["description"].as_str().unwrap_or("").to_string(),
            json,
        });
    }
    Ok(out)
}

/// `max-age=NNN` is recomputed per request on /span, so compare it as a pattern.
fn header_matches(expected: &str, actual: &str) -> bool {
    if expected == actual {
        return true;
    }
    let norm = |s: &str| -> Option<String> {
        let rest = s.strip_prefix("max-age=")?;
        rest.chars().all(|c| c.is_ascii_digit()).then(|| "max-age=<n>".to_string())
    };
    matches!((norm(expected), norm(actual)), (Some(a), Some(b)) if a == b)
}

pub struct Outcome {
    pub name: String,
    pub failures: Vec<String>,
    pub skipped: Option<String>,
}

pub fn run_one(base_url: &str, f: &Fixture) -> Outcome {
    let mut failures = Vec::new();
    let req = &f.json["request"];
    let method = req["method"].as_str().unwrap_or("GET");
    let path = req["path"].as_str().unwrap_or("/");
    let query = req["query"].as_str().unwrap_or("");
    let host = req["headers"]["Host"].as_str().unwrap_or("");
    let url = if query.is_empty() {
        format!("{}{}", base_url.trim_end_matches('/'), path)
    } else {
        format!("{}{}?{}", base_url.trim_end_matches('/'), path, query)
    };

    let mut r = ureq::request(method, &url);
    if !host.is_empty() {
        // The Host header is how the server picks a website; it is part of the request, not
        // of the transport, so it has to survive even when we are talking to 127.0.0.1.
        r = r.set("Host", host);
    }
    r = r.set("User-Agent", "legacy-conformance/0.1");
    // The fixtures record UNCOMPRESSED bodies. Compression is a transport concern that
    // gorilla/handlers.CompressHandler applies to every route, so turn it off rather than
    // depend on the HTTP client to transparently reverse it.
    r = r.set("Accept-Encoding", "identity");
    let resp = match r.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(_, resp)) => resp,
        Err(e) => {
            return Outcome { name: f.name.clone(), failures: vec![format!("request failed: {e}")], skipped: None }
        }
    };

    let status = resp.status();
    let got_headers: Vec<(String, String)> = resp
        .headers_names()
        .into_iter()
        .filter_map(|n| resp.header(&n).map(|v| (n.clone(), v.to_string())))
        .collect();
    let header = |name: &str| -> Option<String> {
        got_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let mut body = Vec::new();
    if let Err(e) = std::io::Read::read_to_end(&mut resp.into_reader(), &mut body) {
        failures.push(format!("could not read body: {e}"));
    }

    let exp = &f.json["response"];
    let asrt = &f.json["assert"];

    if f.b(&["assert", "status"], true) {
        let want = exp["status"].as_u64().unwrap_or(0) as u16;
        if status != want {
            failures.push(format!("status: expected {want}, got {status}"));
        }
    }

    if let Some(names) = asrt["headers"].as_array() {
        for n in names.iter().filter_map(|v| v.as_str()) {
            let want = exp["headers"][n].as_str().unwrap_or("");
            match header(n) {
                None => failures.push(format!("header {n}: MISSING (expected {want:?})")),
                Some(got) if !header_matches(want, &got) => {
                    failures.push(format!("header {n}: expected {want:?}, got {got:?}"))
                }
                Some(_) => {}
            }
        }
    }

    if f.b(&["assert", "trailing_newline"], false) || exp["trailing_newline"].as_bool() == Some(true) {
        let want = exp["trailing_newline"].as_bool().unwrap_or(false);
        let got = body.last() == Some(&b'\n');
        if want != got {
            failures.push(format!(
                "trailing newline: expected {want}, got {got} (json.Encoder.Encode appends one; \
                 raw jsonb passthrough does not)"
            ));
        }
    }

    let recorded_body = exp["body"].as_str();

    if f.b(&["assert", "body_equals"], false) {
        if let Some(want) = recorded_body {
            let got = String::from_utf8_lossy(&body);
            if want != got {
                failures.push(format!("body: expected {want:?}, got {got:?}"));
            }
        }
    }

    if f.b(&["assert", "json_shape"], false) {
        match recorded_body {
            None => {
                return Outcome {
                    name: f.name.clone(),
                    failures,
                    skipped: Some("fixture stores no body to compare against".into()),
                }
            }
            Some(want_text) => {
                let want: Value = match serde_json::from_str(want_text) {
                    Ok(v) => v,
                    Err(e) => {
                        failures.push(format!("fixture body is not valid JSON: {e}"));
                        return Outcome { name: f.name.clone(), failures, skipped: None };
                    }
                };
                match serde_json::from_slice::<Value>(&body) {
                    Err(e) => failures.push(format!(
                        "response body is not valid JSON: {e} (first 200 bytes: {:?})",
                        String::from_utf8_lossy(&body[..body.len().min(200)])
                    )),
                    Ok(got) => {
                        if f.b(&["assert", "numeric_format"], false) {
                            check_number_literals(&got, "$", &mut failures);
                        }
                        match (Shape::of(&want), Shape::of(&got)) {
                            (Err(e), _) => failures.push(format!("fixture shape: {e}")),
                            (_, Err(e)) => failures.push(format!("response shape: {e}")),
                            (Ok(ws), Ok(gs)) => failures.extend(compare(
                                &ws,
                                &gs,
                                f.b(&["assert", "json_key_order"], false),
                            )),
                        }
                    }
                }
            }
        }
    }

    if let Some(subs) = asrt["required_substrings"].as_array() {
        let text = String::from_utf8_lossy(&body);
        for s in subs.iter().filter_map(|v| v.as_str()) {
            if !text.contains(s) {
                failures.push(format!("body is missing required substring {s:?}"));
            }
        }
    }

    Outcome { name: f.name.clone(), failures, skipped: None }
}

pub struct Report {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub text: String,
}

pub fn run_all(base_url: &str, fixtures: &[Fixture]) -> Report {
    let mut r = Report { passed: 0, failed: 0, skipped: 0, text: String::new() };
    for f in fixtures {
        let o = run_one(base_url, f);
        if !o.failures.is_empty() {
            r.failed += 1;
            let _ = writeln!(r.text, "FAIL {}", o.name);
            if !f.description.is_empty() {
                let _ = writeln!(r.text, "     {}", f.description);
            }
            for line in &o.failures {
                let _ = writeln!(r.text, "     - {line}");
            }
        } else if let Some(why) = o.skipped {
            r.skipped += 1;
            let _ = writeln!(r.text, "SKIP {} ({why})", o.name);
        } else {
            r.passed += 1;
            let _ = writeln!(r.text, "ok   {}", o.name);
        }
    }
    let _ = writeln!(r.text, "\n{} passed, {} failed, {} skipped", r.passed, r.failed, r.skipped);
    r
}
