//! Mechanism (c) -- same database, both read paths, diffed responses **by value**.
//!
//! The strongest of §5.0's three mechanisms and the one with a true oracle: after M5-1
//! imported production's history into the new schema, the two read paths answer from
//! equivalent data, so `/span`, `/latest`, `/snow` and `/almanac` can be compared by value
//! rather than by shape. This is where a wrong aggregate shows up, and it is the half the
//! conformance suite cannot do -- DEV-7's own ledger entry says so in as many words.
//!
//! # What is compared, and what is a pairing key rather than a value
//!
//! `ts` is the pairing key on `/span` and `/latest` and is never compared as a value.
//! `/span`'s buckets are paired by it exactly: both sides read the same grid, so a bucket
//! that exists on one side only is a **coverage** difference and is reported as one. DEV-12
//! predicts exactly one of those -- a single extra bucket at the leading edge of the Rust
//! response -- and the allowance is a count from `expected.toml`, applied only at the leading
//! edge. A hole in the middle is never excused by it.
//!
//! `/latest` is the one endpoint where the two sides are serving two different readings: each
//! daemon's own last one, milliseconds apart. They are paired within one station output
//! interval and the offset is reported on the line, because a comparison of two instants an
//! hour apart would be a comparison of the weather against itself.
//!
//! # Nested keys are flattened, so a rule names a leaf
//!
//! `/almanac` carries seventeen records of `{value, timestamp}`, so the comparison walks to
//! the leaves and names them `high_temp.value`. One traversal serves all four endpoints,
//! which is why a shape change on any of them is a reported difference instead of a panic.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;

use crate::census::Censuses;
use crate::clock::format_utc_ms;
use crate::expected::{Candidate, Expected, Scope, Verdict};
use crate::source::HttpSide;
use crate::value::Val;

/// One endpoint, or one band of `/span`.
#[derive(Debug, Clone)]
pub struct Request {
    /// `/span/3h`, `/latest`. What the report prints.
    pub label: String,
    /// The recorded corpus's filename, without the extension.
    pub slug: String,
    pub path: String,
    pub scope: Scope,
    /// The band id for a `/span` request, which is the census's key.
    pub band: Option<String>,
}

/// Every request (c) makes, in the order the report prints them.
pub fn requests(expected: &Expected, station: &str) -> Vec<Request> {
    let q = urlencode(station);
    let mut out = Vec::new();
    for band in &expected.bands {
        out.push(Request {
            label: format!("/span/{}", band.span),
            slug: format!("span-{}", band.span),
            path: format!("/span/{}?station={q}", band.span),
            scope: Scope::Span,
            band: Some(band.id.clone()),
        });
    }
    for (scope, name) in [
        (Scope::Latest, "latest"),
        (Scope::Snow, "snow"),
        (Scope::Almanac, "almanac"),
    ] {
        out.push(Request {
            label: format!("/{name}"),
            slug: name.to_string(),
            path: format!("/{name}?station={q}"),
            scope,
            band: None,
        });
    }
    out
}

/// Percent-encode a station name for a query string. Production has one with spaces in it
/// (`Suncrest Air Quality`), and a raw space makes the request line invalid rather than
/// making the station not found, which is a much more confusing failure.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// What (c) found for one endpoint.
#[derive(Debug, Clone, Default)]
pub struct EndpointOutcome {
    pub label: String,
    pub band: Option<String>,
    pub go_status: u16,
    pub rust_status: u16,
    pub go_elements: usize,
    pub rust_elements: usize,
    pub paired: usize,
    /// Buckets present on the Go side only. Always a defect: the new side is missing data.
    pub go_only: usize,
    /// Buckets present on the Rust side only, excluding the leading ones DEV-12 allows.
    pub rust_only: usize,
    /// Buckets present on both sides that were fed a different number of readings, and so
    /// are not a comparison of the two implementations at all.
    pub uneven: usize,
    /// Leading buckets the Rust side carried alone, and the rule that permits them.
    pub leading_extra: usize,
    pub leading_rule: Option<String>,
    pub keys_compared: u64,
    pub expected: BTreeMap<String, u64>,
    /// Key -> how many elements it differed in, with no rule to predict it.
    pub unexpected: BTreeMap<String, u64>,
    /// Key -> how many elements a rule applied to but had no census for.
    pub no_census: BTreeMap<String, u64>,
    pub key_only_go: BTreeSet<String>,
    pub key_only_rust: BTreeSet<String>,
    /// One printable example per unexpected key.
    pub examples: BTreeMap<String, String>,
    /// Anything a reader needs that is not a count.
    pub notes: Vec<String>,
    /// `/latest` only: how far apart the two readings were.
    pub pair_offset_ms: Option<i64>,
    /// How many values an injection moved in this endpoint's body.
    pub injected: usize,
}

impl EndpointOutcome {
    pub fn is_green(&self) -> bool {
        self.go_status == self.rust_status
            && self.unexpected.is_empty()
            && self.no_census.is_empty()
            && self.key_only_go.is_empty()
            && self.key_only_rust.is_empty()
            && self.go_only == 0
            && self.rust_only == 0
            && self.uneven == 0
            && self.compared_something()
    }

    /// Whether this endpoint is evidence of anything.
    ///
    /// A `200` that yielded no value comparison is not a pass. Without this, seven endpoints
    /// all answering `404 station not found`, or `/span` returning `200 []` on both sides,
    /// report `ok` on every line and `VERDICT (c): PASS` -- and a green run that compared
    /// nothing is exactly docs/reconciliation.md G-1, a gate reporting green by deferral.
    /// Mechanism (b) has had this guard from the start as `paired > 0`.
    ///
    /// A matching NON-2xx status is the one case where comparing nothing is the right answer:
    /// `/snow` 404s for a website with snow disabled, and the two sides agreeing on the
    /// status and the error body is a real agreement about a real response.
    pub fn compared_something(&self) -> bool {
        self.keys_compared > 0 || self.go_status >= 300
    }

    pub fn verdict(&self) -> &'static str {
        if self.is_green() {
            "ok"
        } else {
            "FAIL"
        }
    }
}

/// Flatten a JSON value to dotted leaf paths.
pub fn flatten(v: &Json) -> BTreeMap<String, Val> {
    let mut out = BTreeMap::new();
    walk("", v, &mut out);
    out
}

fn walk(prefix: &str, v: &Json, out: &mut BTreeMap<String, Val>) {
    match v {
        Json::Object(map) => {
            for (k, child) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                walk(&path, child, out);
            }
        }
        Json::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                walk(&format!("{prefix}[{i}]"), child, out);
            }
        }
        // An empty object or array has no leaves at all, so it would otherwise vanish from
        // the comparison -- and `/almanac` returning `{}` against a populated response is
        // precisely the difference worth reporting (the cache is empty on one side).
        leaf => {
            out.insert(prefix.to_string(), Val::from_json(leaf));
        }
    }
}

/// Run (c) for one endpoint.
pub fn diff_endpoint(
    req: &Request,
    go: &HttpSide,
    rust: &HttpSide,
    expected: &Expected,
    census: &Censuses,
    inject: Option<&crate::inject::Injection>,
) -> Result<EndpointOutcome, String> {
    let g = go.fetch(&req.slug, &req.path)?;
    let r = rust.fetch(&req.slug, &req.path)?;
    let mut out = EndpointOutcome {
        label: req.label.clone(),
        band: req.band.clone(),
        go_status: g.status,
        rust_status: r.status,
        ..Default::default()
    };

    if g.status != r.status {
        out.notes.push(format!(
            "status {} on the Go side against {} on the Rust side",
            g.status, r.status
        ));
    }

    // DEV-11, and the only shape it can ever have. `mw_api_legacy::zf` serialises a stored
    // NaN or +/-Inf as the contract's 0; Go's `encoding/json` refuses one mid-stream and
    // truncates the body, so the Go side is not parseable and there is no pair of values to
    // compare. Named from the rule set rather than from a branch here, so that removing the
    // rule removes the excuse.
    let go_nonfinite = g.json.is_none()
        && r.json.is_some()
        && ["NaN", "+Inf", "-Inf", "Infinity"]
            .iter()
            .any(|lit| g.text.contains(lit));

    let (gj, rj) = match (g.json.clone(), r.json.clone()) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            // Four error-body conventions on the Go side and none of them is JSON. Equal
            // text on an equal status is a real agreement and is reported as one.
            if let (true, Some(rule)) = (go_nonfinite, expected.nonfinite_body_rule(req.scope)) {
                out.notes.push(format!(
                    "the Go body is not JSON and carries a non-finite literal, which {rule} \
                     predicts: the new side serialises a stored NaN or infinity as the \
                     contract's 0 and answers, where `encoding/json` refuses it and truncates \
                     the response. Go sent {} bytes of {:?}",
                    g.text.len(),
                    g.text.trim().chars().rev().take(40).collect::<String>()
                ));
                *out.expected.entry(rule.to_string()).or_default() += 1;
                return Ok(out);
            }
            if g.status == r.status && g.text.trim() == r.text.trim() {
                out.notes.push(format!(
                    "both sides: {} {:?}",
                    g.status,
                    g.text.trim().chars().take(60).collect::<String>()
                ));
            } else {
                out.notes.push(format!(
                    "non-JSON bodies differ: go {:?} / rust {:?}",
                    g.text.trim().chars().take(60).collect::<String>(),
                    r.text.trim().chars().take(60).collect::<String>()
                ));
                out.unexpected.insert("<body>".into(), 1);
            }
            return Ok(out);
        }
    };

    let (mut gj, mut rj) = (gj, rj);
    if let Some(inj) = inject {
        out.injected = match inj.side {
            crate::inject::Side::Go => inj.apply_json(&mut gj),
            crate::inject::Side::Rust => inj.apply_json(&mut rj),
        };
    }

    match req.scope {
        Scope::Span => diff_span(&mut out, req, &gj, &rj, expected, census),
        _ => diff_single(&mut out, req, &gj, &rj, expected),
    }
    Ok(out)
}

fn diff_span(
    out: &mut EndpointOutcome,
    req: &Request,
    gj: &Json,
    rj: &Json,
    expected: &Expected,
    census: &Censuses,
) {
    let (Some(ga), Some(ra)) = (gj.as_array(), rj.as_array()) else {
        out.notes
            .push("`/span` did not return an array on one of the two sides".into());
        out.unexpected.insert("<shape>".into(), 1);
        return;
    };
    out.go_elements = ga.len();
    out.rust_elements = ra.len();

    let by_ts = |arr: &[Json]| -> BTreeMap<i64, Json> {
        arr.iter()
            .filter_map(|e| {
                e.get("ts")
                    .and_then(|t| t.as_i64())
                    .map(|ts| (ts, e.clone()))
            })
            .collect()
    };
    let gm = by_ts(ga);
    let rm = by_ts(ra);
    if gm.len() != ga.len() || rm.len() != ra.len() {
        out.notes
            .push("some elements carried no usable `ts` and were not paired".into());
        out.unexpected.insert("ts".into(), 1);
    }

    let (allowed_leading, rule) = expected.allowed_extra_leading_buckets();
    out.leading_rule = rule.map(str::to_string);
    let first_go = gm.keys().next().copied();
    for ts in rm.keys() {
        if gm.contains_key(ts) {
            continue;
        }
        match first_go {
            // Older than anything the Go side returned: DEV-12's extra leading bucket.
            Some(f) if *ts < f => out.leading_extra += 1,
            _ => out.rust_only += 1,
        }
    }
    if out.leading_extra > allowed_leading as usize {
        out.notes.push(format!(
            "{} leading buckets on the Rust side alone, and {} declares at most {}",
            out.leading_extra,
            out.leading_rule.clone().unwrap_or_else(|| "no rule".into()),
            allowed_leading
        ));
        out.rust_only += out.leading_extra - allowed_leading as usize;
        out.leading_extra = allowed_leading as usize;
    }
    out.go_only = gm.keys().filter(|ts| !rm.contains_key(ts)).count();
    if out.go_only > 0 {
        let missing: Vec<i64> = gm
            .keys()
            .filter(|ts| !rm.contains_key(ts))
            .copied()
            .collect();
        out.notes.push(format!(
            "{} buckets present on the Go side only, {} .. {}. The two databases do not hold \
             the same readings over this window, so the values that DID pair are the only \
             part of this line that is a comparison",
            missing.len(),
            format_utc_ms(*missing.first().expect("non-empty")),
            format_utc_ms(*missing.last().expect("non-empty"))
        ));
    }

    let band = req.band.as_deref().unwrap_or("");
    let mut uneven_example = None;
    for (ts, gel) in &gm {
        let Some(rel) = rm.get(ts) else { continue };
        // An unevenly populated bucket is a coverage difference and is counted as one. Its
        // values are deliberately NOT compared: every statistic in it would differ, and
        // thirteen reported value differences would bury the one fact that explains them.
        if let Some((a, b)) = census.uneven(band, *ts) {
            out.uneven += 1;
            uneven_example.get_or_insert(format!(
                "{}: {a} readings on the Go side against {b} on the Rust side",
                format_utc_ms(*ts)
            ));
            continue;
        }
        out.paired += 1;
        let n = census.n_go(band, *ts);
        compare_maps(
            out,
            req.scope,
            &flatten(gel),
            &flatten(rel),
            expected,
            n,
            Some(*ts),
        );
    }
    if let Some(example) = uneven_example {
        out.notes.push(format!(
            "{} bucket(s) exist on both sides but were fed a different number of readings, so \
             their values are not compared -- e.g. {example}",
            out.uneven
        ));
    }
}

fn diff_single(
    out: &mut EndpointOutcome,
    req: &Request,
    gj: &Json,
    rj: &Json,
    expected: &Expected,
) {
    out.go_elements = 1;
    out.rust_elements = 1;
    let gf = flatten(gj);
    let rf = flatten(rj);

    if req.scope == Scope::Latest {
        // Each side is serving its own daemon's last reading. Pair them or say why not.
        let ts = |m: &BTreeMap<String, Val>| m.get("ts").and_then(Val::num).map(|f| f as i64);
        match (ts(&gf), ts(&rf)) {
            (Some(a), Some(b)) => {
                let offset = (a - b).abs();
                out.pair_offset_ms = Some(offset);
                if offset > expected.latest_pair_tolerance_ms {
                    out.notes.push(format!(
                        "the two sides' last readings are {} ms apart, beyond the declared {} ms: \
                         {} on the Go side, {} on the Rust side. Nothing below is a comparison \
                         of the same reading",
                        offset,
                        expected.latest_pair_tolerance_ms,
                        format_utc_ms(a),
                        format_utc_ms(b)
                    ));
                    out.unexpected.insert("ts".into(), 1);
                    return;
                }
            }
            _ => {
                out.notes
                    .push("`/latest` carried no `ts` on one of the two sides".into());
                out.unexpected.insert("ts".into(), 1);
                return;
            }
        }
    }

    if gf.is_empty() && rf.is_empty() {
        out.notes.push(
            "both sides returned an empty body, so nothing was compared. An agreement about \
             the absence of data is not an agreement about data"
                .into(),
        );
        return;
    }
    out.paired = 1;
    compare_maps(out, req.scope, &gf, &rf, expected, None, None);
}

fn compare_maps(
    out: &mut EndpointOutcome,
    scope: Scope,
    gf: &BTreeMap<String, Val>,
    rf: &BTreeMap<String, Val>,
    expected: &Expected,
    n_readings: Option<u64>,
    ts: Option<i64>,
) {
    for key in gf.keys() {
        if !rf.contains_key(key) {
            out.key_only_go.insert(key.clone());
        }
    }
    for key in rf.keys() {
        if !gf.contains_key(key) {
            out.key_only_rust.insert(key.clone());
        }
    }
    for (key, gv) in gf {
        let Some(rv) = rf.get(key) else { continue };
        // The pairing key is not a value. Its agreement is reported as the pairing, and
        // comparing it here would make every `/latest` line red for a 3 ms scheduling delay.
        if expected.not_values.contains(key) {
            continue;
        }
        out.keys_compared += 1;
        let verdict = expected.classify(&Candidate {
            scope,
            key,
            go: gv,
            rust: rv,
            n_readings,
            row: None,
        });
        match verdict {
            Verdict::Equal => {}
            Verdict::Expected(id) => *out.expected.entry(id).or_default() += 1,
            Verdict::NoCensus(id) => {
                *out.no_census.entry(key.clone()).or_default() += 1;
                out.examples.entry(key.clone()).or_insert_with(|| {
                    format!("{id} applies but the bucket has no census entry: go={gv} rust={rv}")
                });
            }
            Verdict::Unexpected => {
                *out.unexpected.entry(key.clone()).or_default() += 1;
                out.examples.entry(key.clone()).or_insert_with(|| match ts {
                    Some(t) => format!("{} go={gv} rust={rv}", format_utc_ms(t)),
                    None => format!("go={gv} rust={rv}"),
                });
            }
        }
    }
}
