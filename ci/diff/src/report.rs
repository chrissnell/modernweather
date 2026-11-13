//! The summary, written for somebody who is not the author.
//!
//! M5-2's instruction: "One line per endpoint and band, the expected diffs marked as
//! expected, anything else loud. A harness whose output only its author can interpret has not
//! finished the job." So: a provenance block naming the two systems the verdict is about, one
//! table row per unit of comparison, then a loud block per failure with an example in it, then
//! one verdict line.
//!
//! Two rules it follows that are easy to get wrong. **Expected differences are printed, not
//! hidden** -- `DEV-4:44` on a line means the rule fired forty-four times, and a rule that
//! stopped firing is as interesting as one that started. And **a count of zero is printed as
//! `0`**, because a blank cell reads as "not checked" and the whole claim of this harness is
//! that everything was.

use std::collections::BTreeMap;

use crate::clock::format_utc_ms;
use crate::readings::Outcome as ReadingsOutcome;
use crate::responses::EndpointOutcome;

/// What the run was pointed at, for the block above the table.
pub struct Provenance {
    pub lines: Vec<(String, String)>,
}

impl Provenance {
    pub fn render(&self) -> String {
        let width = self.lines.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        self.lines
            .iter()
            .map(|(k, v)| format!("  {k:width$}  {v}\n"))
            .collect()
    }
}

fn rules(m: &BTreeMap<String, u64>) -> String {
    if m.is_empty() {
        return "-".into();
    }
    m.iter()
        .map(|(k, n)| format!("{k}:{n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Mechanism (b)'s report.
pub fn readings(out: &ReadingsOutcome, prov: &Provenance, verbose: bool) -> String {
    let mut s = String::new();
    s.push_str("=== mechanism (b) -- same station, both daemons, diffed readings\n\n");
    s.push_str(&prov.render());
    s.push('\n');
    s.push_str(&format!(
        "  readings   go {}   rust {}   paired {}   unpaired go {}   unpaired rust {}   ambiguous {}\n",
        out.go.rows.len(),
        out.rust.rows.len(),
        out.paired,
        out.unpaired_go,
        out.unpaired_rust,
        out.ambiguous
    ));
    s.push_str(&format!(
        "  pairing    largest offset used {} ms\n",
        out.max_offset_ms
    ));
    s.push_str(&format!(
        "  columns    compared {}   go only {}   rust only {}\n",
        out.compared_columns.len(),
        out.go_only_columns.len(),
        out.rust_only_columns.len()
    ));
    if !out.go_only_columns.is_empty() {
        s.push_str(&format!(
            "             go only: {}\n",
            out.go_only_columns.join(", ")
        ));
    }
    if !out.rust_only_columns.is_empty() {
        s.push_str(&format!(
            "             rust only: {}\n",
            out.rust_only_columns.join(", ")
        ));
    }
    for (label, side) in [("go", &out.go), ("rust", &out.rust)] {
        if !side.non_numeric.is_empty() {
            s.push_str(&format!(
                "             {label} not numeric, not compared: {}\n",
                side.non_numeric.join(", ")
            ));
        }
    }

    s.push('\n');
    s.push_str("  metric                      compared    equal  expected  unexpected\n");
    s.push_str("  --------------------------------------------------------------------\n");
    // Rolled up by what the harness concluded about the metric, not by metric name: a reader
    // asking "what differed" wants four lines and a list, and 197 rows of zeroes is the
    // output M5-2 warns about -- "a harness whose output only its author can interpret".
    // `--verbose` prints the full table, which is what a reviewer checking the roll-up wants.
    let mut equal_only: Vec<&str> = Vec::new();
    let mut by_rule: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (metric, stat) in &out.stats {
        let expected_total: u64 = stat.expected.values().sum();
        if stat.unexpected == 0 {
            if expected_total == 0 {
                equal_only.push(metric);
            } else {
                // Keyed by the rule ids and not by `rules()`, whose counts differ per
                // metric: `windchill` matched DEV-9 fifty-two times and `heatindex` six
                // hundred, and grouping on the counts would print one line each.
                let ids: Vec<&str> = stat.expected.keys().map(String::as_str).collect();
                by_rule.entry(ids.join(" ")).or_default().push(metric);
            }
            if !verbose {
                continue;
            }
        }
        s.push_str(&format!(
            "  {:<24} {:>10} {:>8} {:>9} {:>11}{}\n",
            metric,
            stat.compared,
            stat.equal,
            expected_total,
            stat.unexpected,
            if stat.unexpected > 0 { "  FAIL" } else { "" }
        ));
        if stat.unexpected > 0 {
            if let Some(offset) = stat.constant_offset() {
                s.push_str(&format!(
                    "  {:<24}   rust - go = {:+} on every differing reading, which is one \
                     calibration applied on one side and not the other rather than a \
                     distribution of errors\n",
                    "", offset
                ));
            }
            if let Some(offset) = stat.constant_offset_mod360() {
                s.push_str(&format!(
                    "  {:<24}   rust - go = {:+} modulo 360 on every differing reading: a \
                     bearing offset configured on one side and not the other\n",
                    "", offset
                ));
            }
            if let Some((ts, g, r)) = &stat.example {
                s.push_str(&format!(
                    "  {:<24}   e.g. {} go={g} rust={r}\n",
                    "",
                    format_utc_ms(*ts)
                ));
            }
        }
    }
    s.push_str("  --------------------------------------------------------------------\n");
    let name_list = |v: &[&str]| -> String {
        if v.len() <= 12 {
            format!(": {}", v.join(", "))
        } else {
            String::new()
        }
    };
    if !equal_only.is_empty() {
        s.push_str(&format!(
            "  {} metric(s) equal on every paired reading{}\n",
            equal_only.len(),
            name_list(&equal_only)
        ));
    }
    for (rule, metrics) in &by_rule {
        s.push_str(&format!(
            "  {} metric(s) whose every difference is {}{}\n",
            metrics.len(),
            rule,
            name_list(metrics)
        ));
    }
    s.push('\n');
    if out.is_green() {
        s.push_str(&format!(
            "  VERDICT (b): PASS -- {} readings paired, and the only differences are the \
             NULL-versus-zero class D-26 predicts\n",
            out.paired
        ));
    } else {
        let mut why: Vec<String> = Vec::new();
        let unexpected = out.unexpected_metrics();
        if !unexpected.is_empty() {
            why.push(format!(
                "{} metric(s) differ for a reason no rule predicts: {}",
                unexpected.len(),
                unexpected
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if out.paired == 0 {
            why.push("nothing paired, so nothing was compared".into());
        }
        if out.unpaired_go > 0 || out.unpaired_rust > 0 {
            why.push(format!(
                "{} Go and {} Rust readings had no partner, so the two daemons did not see \
                 the same frames over this window",
                out.unpaired_go, out.unpaired_rust
            ));
        }
        if out.ambiguous > 0 {
            why.push(format!(
                "{} readings had more than one candidate partner",
                out.ambiguous
            ));
        }
        if !out.go_only_columns.is_empty() || !out.rust_only_columns.is_empty() {
            why.push("the two schemas do not carry the same columns".into());
        }
        s.push_str("  VERDICT (b): FAIL\n");
        for w in why {
            s.push_str(&format!("    - {w}\n"));
        }
    }
    s
}

/// Mechanism (c)'s report.
pub fn responses(outs: &[EndpointOutcome], prov: &Provenance) -> String {
    let mut s = String::new();
    s.push_str("=== mechanism (c) -- same database, both read paths, diffed responses\n\n");
    s.push_str(&prov.render());
    s.push('\n');
    s.push_str(
        "  endpoint      band   status     elements    paired  uneven  go-only  rust-only     keys  expected             verdict\n",
    );
    s.push_str(
        "  ------------------------------------------------------------------------------------------------------------------\n",
    );
    for o in outs {
        s.push_str(&format!(
            "  {:<13} {:<6} {:>3}/{:<3}   {:>5}/{:<5} {:>7} {:>7} {:>8} {:>10} {:>8}  {:<20} {}\n",
            o.label,
            o.band.clone().unwrap_or_else(|| "-".into()),
            o.go_status,
            o.rust_status,
            o.go_elements,
            o.rust_elements,
            o.paired,
            o.uneven,
            o.go_only,
            o.rust_only,
            o.keys_compared,
            rules(&o.expected),
            o.verdict()
        ));
    }
    s.push_str(
        "  ------------------------------------------------------------------------------------------------------------------\n",
    );

    for o in outs {
        if o.is_green() && o.notes.is_empty() && o.leading_extra == 0 {
            continue;
        }
        s.push_str(&format!("\n  {} {}\n", o.verdict(), o.label));
        if o.leading_extra > 0 {
            s.push_str(&format!(
                "    {} leading bucket(s) on the Rust side alone, which {} predicts\n",
                o.leading_extra,
                o.leading_rule.clone().unwrap_or_else(|| "no rule".into())
            ));
        }
        if let Some(offset) = o.pair_offset_ms {
            s.push_str(&format!("    the two readings are {offset} ms apart\n"));
        }
        for note in &o.notes {
            s.push_str(&format!("    {note}\n"));
        }
        if !o.key_only_go.is_empty() {
            s.push_str(&format!(
                "    keys present on the Go side only: {}\n",
                o.key_only_go.iter().cloned().collect::<Vec<_>>().join(" ")
            ));
        }
        if !o.key_only_rust.is_empty() {
            s.push_str(&format!(
                "    keys present on the Rust side only: {}\n",
                o.key_only_rust
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        if !o.no_census.is_empty() {
            s.push_str(&format!(
                "    no census, so a rule's bound could not be applied: {}\n",
                o.no_census
                    .iter()
                    .map(|(k, n)| format!("{k}({n})"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        if !o.unexpected.is_empty() {
            s.push_str(&format!(
                "    unexpected: {}\n",
                o.unexpected
                    .iter()
                    .map(|(k, n)| format!("{k}({n})"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
            for key in o.unexpected.keys() {
                if let Some(e) = o.examples.get(key) {
                    s.push_str(&format!("      {key:<16} {e}\n"));
                }
            }
        }
    }

    let failed: Vec<&EndpointOutcome> = outs.iter().filter(|o| !o.is_green()).collect();
    s.push('\n');
    if failed.is_empty() {
        s.push_str(&format!(
            "  VERDICT (c): PASS -- {} endpoints and bands, and the only differences are the \
             ones expected.toml predicts\n",
            outs.len()
        ));
    } else {
        s.push_str(&format!(
            "  VERDICT (c): FAIL -- {} of {} endpoints and bands carry differences no rule \
             predicts: {}\n",
            failed.len(),
            outs.len(),
            failed
                .iter()
                .map(|o| o.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    s
}
