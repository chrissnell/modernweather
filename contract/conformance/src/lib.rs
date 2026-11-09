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
//!
//! # Three classes, and a deviation a target may be asked to apply
//!
//! Each fixture carries a `class` -- `frozen`, `coupled` or `retired` -- which is work item
//! **N-18** and `docs/reconciliation.md` §6.4's classification. This runner **reports** it
//! and replays every fixture regardless; what a class *means for a gate* is the gate's
//! policy and lives in `xtask/src/conformance.rs`. The split is deliberate: the runner is a
//! measuring instrument and a classification that narrowed the replay would be exactly the
//! silent filter the `attempted == 70` assertion exists to prevent.
//!
//! A fixture may also carry a `deviation` block: a second recorded response, with an id,
//! for a target that is **known** to differ on purpose. [`run_all`] applies one only when
//! its id is in the enabled set, so the Go-target replay keeps asserting what shipped while
//! the Rust-target replay asserts what the deviation says. One fixture holds both halves,
//! which is the only arrangement under which both gates keep biting.

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

/// Which class a fixture's endpoint is in (`docs/reconciliation.md` §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    /// One of the five endpoints the iOS client calls. A diff is a contract break.
    Frozen,
    /// One of the website's. A diff is a lockstep change, allowed in one PR with the
    /// embedded JavaScript.
    Coupled,
    /// `/alerts`, `/api/remote-stations` or `format=msgpack`: not carried forward (D-69).
    /// The fixture is kept so that the replay still attempts all 70.
    Retired,
}

impl Class {
    /// The name the fixture file carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Frozen => "frozen",
            Class::Coupled => "coupled",
            Class::Retired => "retired",
        }
    }

    /// Parse a fixture's `class`.
    pub fn parse(name: &str) -> Option<Class> {
        match name {
            "frozen" => Some(Class::Frozen),
            "coupled" => Some(Class::Coupled),
            "retired" => Some(Class::Retired),
            _ => None,
        }
    }
}

pub struct Fixture {
    pub name: String,
    pub description: String,
    /// Which class this fixture's endpoint is in.
    pub class: Class,
    /// The work item that will implement this fixture's subject, for one no landed item
    /// has. `None` once it has landed.
    pub pending: Option<String>,
    /// The deviation id this fixture carries a second recorded response for, if any.
    pub deviation: Option<String>,
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

/// Load every fixture in `dir`, in filename order.
///
/// **A fixture without a `class` is an error rather than a default.** Defaulting it would
/// make an unclassified fixture silently gate (or silently not), which is the thing the
/// classification exists to make visible.
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
        let name = json["name"].as_str().unwrap_or("<unnamed>").to_string();
        // An error rather than a panic, so that `main` reports it and exits 2 -- the code
        // that means "this run proved nothing" -- instead of a backtrace.
        let class = match json["class"].as_str().and_then(Class::parse) {
            Some(class) => class,
            None => {
                return Err(std::io::Error::other(format!(
                    "{}: no `class` (frozen, coupled or retired). Work item N-18 requires \
                     one on every fixture; `contract/tools/capture.py` declares it on the \
                     `spec(...)` line.",
                    p.display()
                )))
            }
        };
        out.push(Fixture {
            description: json["description"].as_str().unwrap_or("").to_string(),
            class,
            pending: json["pending"].as_str().map(str::to_string),
            deviation: json["deviation"]["id"].as_str().map(str::to_string),
            name,
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
    /// Which class the fixture is in, carried through so the report can group by it.
    pub class: Class,
    pub failures: Vec<String>,
    pub skipped: Option<String>,
    /// The deviation id that was applied, if one was.
    pub deviation: Option<String>,
}

/// Replay one fixture.
///
/// `deviations` is the set of deviation ids this target is known to differ by on purpose.
/// When a fixture declares one of them, its `deviation.response` is compared against
/// instead of its `response` -- see the module documentation.
pub fn run_one(base_url: &str, f: &Fixture, deviations: &[String]) -> Outcome {
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
            return Outcome {
                name: f.name.clone(),
                class: f.class,
                failures: vec![format!("request failed: {e}")],
                skipped: None,
                deviation: None,
            }
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

    // Which recording this target is measured against. The assertion SET is the fixture's
    // either way: a deviation changes the bytes a target is expected to produce, not which
    // properties are checked.
    let applied = f
        .deviation
        .as_ref()
        .filter(|id| deviations.iter().any(|enabled| enabled == *id));
    let exp = match applied {
        Some(_) => &f.json["deviation"]["response"],
        None => &f.json["response"],
    };
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
                    class: f.class,
                    failures,
                    skipped: Some("fixture stores no body to compare against".into()),
                    deviation: applied.cloned(),
                }
            }
            Some(want_text) => {
                let want: Value = match serde_json::from_str(want_text) {
                    Ok(v) => v,
                    Err(e) => {
                        failures.push(format!("fixture body is not valid JSON: {e}"));
                        return Outcome {
                            name: f.name.clone(),
                            class: f.class,
                            failures,
                            skipped: None,
                            deviation: applied.cloned(),
                        };
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

    Outcome {
        name: f.name.clone(),
        class: f.class,
        failures,
        skipped: None,
        deviation: applied.cloned(),
    }
}

pub struct Report {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    /// One entry per class, in `Frozen`, `Coupled`, `Retired` order.
    pub by_class: Vec<ClassReport>,
    /// Every deviation id that was applied to at least one fixture.
    pub applied: Vec<String>,
    /// Every fixture's verdict, for a caller that applies a policy per class.
    pub verdicts: Vec<Verdict>,
    pub text: String,
}

/// One class's counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassReport {
    pub class: Class,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
}

/// What one fixture did, reduced to what a gate's policy needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub name: String,
    pub class: Class,
    /// The work item that will implement this fixture's subject, when no landed one has.
    pub pending: Option<String>,
    pub passed: bool,
    pub skipped: bool,
}

/// Replay every fixture, applying the enabled deviations.
///
/// Every fixture is replayed whatever its class: a class is **reported** and never a filter,
/// which is the property `xtask`'s `attempted == 70` assertion is built on.
pub fn run_all(base_url: &str, fixtures: &[Fixture], deviations: &[String]) -> Report {
    let mut r = Report {
        passed: 0,
        failed: 0,
        skipped: 0,
        by_class: Vec::new(),
        applied: Vec::new(),
        verdicts: Vec::new(),
        text: String::new(),
    };
    let mut counts: BTreeMap<Class, (usize, usize, usize)> = BTreeMap::new();
    for f in fixtures {
        let o = run_one(base_url, f, deviations);
        let entry = counts.entry(f.class).or_insert((0, 0, 0));
        if let Some(id) = &o.deviation {
            if !r.applied.iter().any(|a| a == id) {
                r.applied.push(id.clone());
            }
        }
        // The class and the applied deviation are on every line, so a reader of the log can
        // tell a frozen-class failure from a coupled-class one without a second lookup --
        // which is the half of N-18 that is about the report rather than the data.
        let tag = match &o.deviation {
            Some(id) => format!("[{} {id}]", f.class.as_str()),
            None => format!("[{}]", f.class.as_str()),
        };
        let (passed, skipped) = if !o.failures.is_empty() {
            r.failed += 1;
            entry.1 += 1;
            let _ = writeln!(r.text, "FAIL {} {tag}", o.name);
            if let Some(pending) = &f.pending {
                let _ = writeln!(r.text, "     not implemented yet: {pending}");
            }
            if !f.description.is_empty() {
                let _ = writeln!(r.text, "     {}", f.description);
            }
            for line in &o.failures {
                let _ = writeln!(r.text, "     - {line}");
            }
            (false, false)
        } else if let Some(why) = o.skipped {
            r.skipped += 1;
            entry.2 += 1;
            let _ = writeln!(r.text, "SKIP {} {tag} ({why})", o.name);
            (false, true)
        } else {
            r.passed += 1;
            entry.0 += 1;
            let _ = writeln!(r.text, "ok   {} {tag}", o.name);
            (true, false)
        };
        r.verdicts.push(Verdict {
            name: o.name,
            class: f.class,
            pending: f.pending.clone(),
            passed,
            skipped,
        });
    }
    r.by_class = counts
        .into_iter()
        .map(|(class, (passed, failed, skipped))| ClassReport {
            class,
            passed,
            failed,
            skipped,
        })
        .collect();
    r.text.push('\n');
    for c in &r.by_class {
        // A per-class line, which is what makes a frozen-class failure visibly different
        // from a coupled-class one (**N-18**). Deliberately NOT in the shape of the summary
        // line below: `xtask` reads the last line matching `N passed, M failed, K skipped`
        // and a per-class line in that shape would be read as the whole report.
        let _ = writeln!(
            r.text,
            "class {:<8} {} passed / {} failed / {} skipped",
            c.class.as_str(),
            c.passed,
            c.failed,
            c.skipped
        );
    }
    if !r.applied.is_empty() {
        let _ = writeln!(r.text, "deviations applied: {}", r.applied.join(", "));
    }
    let _ = writeln!(
        r.text,
        "\n{} passed, {} failed, {} skipped",
        r.passed, r.failed, r.skipped
    );
    r
}

/// Every deviation id any fixture declares.
///
/// `--deviation` is checked against this, so a typo is a refusal rather than a flag that
/// silently enables nothing -- which would make the Rust target quietly assert the Go
/// behaviour and fail for a reason that does not name itself.
pub fn declared_deviations(fixtures: &[Fixture]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in fixtures {
        if let Some(id) = &f.deviation {
            if !out.iter().any(|a| a == id) {
                out.push(id.clone());
            }
        }
    }
    out.sort();
    out
}
