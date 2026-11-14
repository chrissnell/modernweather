//! The two gates the management UI creates: `ui-bundle` and `ui-copy`.
//!
//! Both are declared in `ci/gates.toml` with `arms_when` paths that **this work item
//! creates** -- `ui/budget.toml` and `ui/src/lib/copy/index.ts` -- which is W8's mechanism
//! for a check whose subject arrives after the gate: the work item that creates the file
//! turns the gate on, so nobody has to remember to. See docs/reconciliation.md G-4 for
//! what happens to a decision with no mechanism behind it.
//!
//! Wrapped as `xtask` gates rather than as `pnpm` scripts so that there is one gate
//! vocabulary across the repository (§6.4), and so that `cargo xtask gates` prints their
//! armed state into every run's summary.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Deserialize;

// =======================================================================================
// ui-bundle
// =======================================================================================

#[derive(Debug, Deserialize)]
struct BudgetFile {
    budget: Budget,
    #[serde(default)]
    prohibited: Prohibited,
}

#[derive(Debug, Deserialize)]
struct Budget {
    initial_route_kb: u64,
    all_routes_kb: u64,
    largest_lazy_chunk_kb: u64,
    fonts_kb: u64,
}

#[derive(Debug, Default, Deserialize)]
struct Prohibited {
    #[serde(default)]
    date_libraries: Vec<String>,
    #[serde(default)]
    cdn_hosts: Vec<String>,
    #[serde(default)]
    chart_libraries: Vec<String>,
}

/// One measured file.
#[derive(Debug, Clone)]
struct Measured {
    path: String,
    gzipped: u64,
}

fn gzipped_size(bytes: &[u8]) -> u64 {
    // Default compression, which is what a server serving `Content-Encoding: gzip` without
    // a tuned level produces -- so the number the gate enforces is the number a browser
    // actually downloads rather than the best case.
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = encoder.write_all(bytes);
    encoder
        .finish()
        .map(|v| v.len() as u64)
        .unwrap_or(bytes.len() as u64)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The files `index.html` asks the browser to fetch **before** anything is rendered.
///
/// §6.5 defines the initial route as "JS + CSS needed to render Home after sign-in", and
/// SvelteKit expresses exactly that as the `modulepreload` and stylesheet links in the
/// fallback document: the entry, the root layout's node, Home's node and the chunks they
/// share. Parsing the document is therefore more honest than naming a directory -- it
/// measures what the browser is told to fetch, which is the thing a budget is about.
fn initial_route_paths(index_html: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some((_, tail)) = index_html.split_once("<head") {
        let head = tail.split_once("</head>").map_or(tail, |(h, _)| h);
        let mut rest = head;
        while let Some(at) = rest.find("href=\"") {
            rest = &rest[at + 6..];
            let Some(end) = rest.find('"') else { break };
            let href = &rest[..end];
            if href.ends_with(".js") || href.ends_with(".css") {
                out.insert(href.trim_start_matches('/').to_string());
            }
            rest = &rest[end..];
        }
    }
    out
}

const KB: u64 = 1024;

/// `cargo xtask gate-ui-bundle`.
///
/// Exit codes, because the fixes are different and C-W2-20 is the precedent for saying so:
///
/// * `1` -- over budget. Make the bundle smaller, or raise a number in `ui/budget.toml`
///   and have the diff reviewed.
/// * `2` -- the gate reached no verdict: no bundle, or an unreadable budget file. It
///   proved nothing either way, and reporting that as "over budget" would be the expensive
///   direction to lie in.
/// * `3` -- a prohibition was violated: a date library, a CDN reference or a chart library.
///   Not a size failure and not fixed by raising a number.
pub fn bundle(root: &Path) -> ExitCode {
    let budget_path = root.join("ui/budget.toml");
    let text = match std::fs::read_to_string(&budget_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ui-bundle: reading {}: {e}", budget_path.display());
            return ExitCode::from(2);
        }
    };
    let file: BudgetFile = match toml::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("ui-bundle: parsing {}: {e}", budget_path.display());
            return ExitCode::from(2);
        }
    };

    let build = root.join("ui/build");
    let index = build.join("index.html");
    let index_html = match std::fs::read_to_string(&index) {
        Ok(t) => t,
        Err(_) => {
            eprintln!(
                "ui-bundle: there is no bundle at {}.\n\n\
                 The gate reached no verdict rather than passing, because a budget check \
                 that reports green without measuring anything is the defect it exists \
                 downstream of. Build it first:\n\n    \
                 cd ui && pnpm install --frozen-lockfile && pnpm build\n\n\
                 `cargo xtask ui` does exactly that, and `cargo xtask dist` runs it \
                 natively before the cross matrix (D-58).",
                build.display()
            );
            return ExitCode::from(2);
        }
    };

    let mut files = Vec::new();
    walk(&build, &mut files);

    let initial_paths = initial_route_paths(&index_html);
    let mut initial: Vec<Measured> = Vec::new();
    let mut lazy: Vec<Measured> = Vec::new();
    let mut fonts: u64 = 0;

    for path in &files {
        let relative = path
            .strip_prefix(&build)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if relative.ends_with(".woff2") || relative.ends_with(".woff") {
            // Fonts are counted raw: `woff2` is already compressed, so gzipping it again
            // is what the measurement would be lying about.
            fonts += bytes.len() as u64;
            continue;
        }
        if !(relative.ends_with(".js") || relative.ends_with(".css")) {
            continue;
        }
        let measured = Measured {
            path: relative.clone(),
            gzipped: gzipped_size(&bytes),
        };
        if initial_paths.contains(&relative) {
            initial.push(measured);
        } else {
            lazy.push(measured);
        }
    }

    if initial.is_empty() {
        eprintln!(
            "ui-bundle: `index.html` names no JavaScript or CSS, so there is nothing to \
             measure. The gate reached no verdict."
        );
        return ExitCode::from(2);
    }

    let initial_total: u64 = initial.iter().map(|m| m.gzipped).sum();
    let all_total: u64 = initial_total + lazy.iter().map(|m| m.gzipped).sum::<u64>();
    let largest_lazy = lazy.iter().map(|m| m.gzipped).max().unwrap_or(0);

    let mut all: Vec<Measured> = initial.iter().chain(lazy.iter()).cloned().collect();
    all.sort_by_key(|m| std::cmp::Reverse(m.gzipped));

    println!("The management UI's bundle, gzipped, against ui/budget.toml.\n");
    println!(
        "  {:<24} {:>11} {:>11} {:>11}",
        "", "measured", "budget", "headroom"
    );
    let rows: [(&str, u64, u64); 4] = [
        (
            "initial route",
            initial_total,
            file.budget.initial_route_kb * KB,
        ),
        ("all routes", all_total, file.budget.all_routes_kb * KB),
        (
            "largest lazy chunk",
            largest_lazy,
            file.budget.largest_lazy_chunk_kb * KB,
        ),
        ("fonts", fonts, file.budget.fonts_kb * KB),
    ];
    let mut over: Vec<String> = Vec::new();
    for (name, measured, limit) in rows {
        let headroom = limit as i64 - measured as i64;
        println!(
            "  {:<24} {:>8} KB {:>8} KB {:>8} KB{}",
            name,
            measured / KB,
            limit / KB,
            headroom / KB as i64,
            if headroom < 0 { "   OVER" } else { "" }
        );
        if headroom < 0 {
            over.push(format!(
                "{name}: {} KB against a budget of {} KB",
                measured / KB,
                limit / KB
            ));
        }
    }

    println!("\n  the five largest files");
    for measured in all.iter().take(5) {
        println!("    {:>8} KB  {}", measured.gzipped / KB, measured.path);
    }
    println!(
        "\n  {} files in the initial route, {} lazily loaded.",
        initial.len(),
        lazy.len()
    );
    // **Deliberately not a delta against the previous commit**, which W5 §6.5 asks for:
    // producing one means building the previous commit's bundle as well, which doubles the
    // job for information the headroom column already carries. Recorded as a deviation in
    // docs/reconciliation.md rather than left as a silent omission.
    println!("  Headroom is the delta that matters; raising a budget is a reviewed diff.");

    // The three prohibitions. Checked by name against what was actually built and against
    // the dependency manifest, because "we did not add moment.js" is true until somebody
    // does.
    let mut violations: Vec<String> = Vec::new();
    let manifest = std::fs::read_to_string(root.join("ui/package.json")).unwrap_or_default();
    for name in file
        .prohibited
        .date_libraries
        .iter()
        .chain(file.prohibited.chart_libraries.iter())
    {
        if manifest.contains(&format!("\"{name}\"")) {
            violations.push(format!(
                "`{name}` is in ui/package.json. W5 Decision 51 prohibits it."
            ));
        }
    }
    let built: String = files
        .iter()
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e == "js" || e == "css" || e == "html")
        })
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect();
    for host in &file.prohibited.cdn_hosts {
        if built.contains(host.as_str()) {
            violations.push(format!(
                "the built bundle references `{host}`. W5 Decision 51 prohibits a CDN: a \
                 weather station on a LAN with no internet must have a working admin UI, \
                 and a CDN is also a third party that learns when the operator signs in."
            ));
        }
    }

    if !violations.is_empty() {
        eprintln!("\nui-bundle: a prohibition was violated.\n");
        for v in &violations {
            eprintln!("  - {v}");
        }
        return ExitCode::from(3);
    }

    if !over.is_empty() {
        eprintln!("\nui-bundle: over budget.\n");
        for o in &over {
            eprintln!("  - {o}");
        }
        eprintln!(
            "\nMake it smaller, or raise the number in ui/budget.toml and have the diff \
             reviewed -- which is the point of the budget being a file."
        );
        return ExitCode::from(1);
    }

    println!("\nui-bundle: inside budget.");
    ExitCode::SUCCESS
}

// =======================================================================================
// ui-copy
// =======================================================================================

/// §0.1's list, plus every `Capability` variant name (§6.6).
///
/// `device` is on it and `station` is not: the current UI alternates between the two
/// **within a single screen**, and this lint is what stops that recurring. "Website" and
/// "station" are the operator's own words and are deliberately absent.
const BANNED: &[&str] = &[
    // §0.1's own list.
    "component",
    "capability",
    "trigger",
    "sink",
    "provision",
    "envelope",
    "spool",
    "readingstore",
    "windowedview",
    "latestsnapshot",
    "httproutes",
    "controller",
    "storage backend",
    "engine",
    "device",
    "supervisor",
    // The `Capability` variants, which §6.6 adds by name.
    "readingstream",
    "snapshottable",
    "scheduleclock",
    "peerregistry",
    "stationregistry",
    "routetable",
    "logcontrol",
    "componentcontrol",
    "configadmin",
];

/// A hit, with enough context to fix it without opening the file.
#[derive(Debug)]
struct Hit {
    file: String,
    line: usize,
    term: String,
    text: String,
}

/// Where the escape hatch is permitted, and nowhere else.
const RAW_MODULE: &str = "src/lib/copy/advanced.ts";

/// `cargo xtask gate-ui-copy`.
///
/// Exit codes:
///
/// * `1` -- an internal term reached a user-facing string. D-36 says that is a bug.
/// * `2` -- no verdict: the copy modules are missing, so nothing was checked.
/// * `3` -- `raw()` was used outside `System → Advanced`'s module. The escape hatch is a
///   greppable list of exceptions or it is a hole in the rule; it cannot be both.
pub fn copy(root: &Path) -> ExitCode {
    let ui = root.join("ui");
    let copy_dir = ui.join("src/lib/copy");
    if !copy_dir.join("index.ts").is_file() {
        eprintln!(
            "ui-copy: there is no copy module at {}. The gate reached no verdict.",
            copy_dir.display()
        );
        return ExitCode::from(2);
    }

    let mut files = Vec::new();
    walk(&ui.join("src"), &mut files);
    files.retain(|p| p.extension().is_some_and(|e| e == "ts" || e == "svelte"));
    files.sort();

    let mut hits: Vec<Hit> = Vec::new();
    let mut exceptions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut misplaced_raw: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for path in &files {
        let relative = path
            .strip_prefix(&ui)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };

        // The escape hatch, recorded and located. Read from the text with its comments
        // removed, because the modules that *explain* the escape hatch write `raw('...')`
        // in a comment -- and a gate that cannot tell an explanation from a use would make
        // documenting the rule a violation of it.
        let code = strip_comments(&text, relative.ends_with(".svelte"));
        for raw in raw_terms(&code) {
            if relative == RAW_MODULE {
                exceptions.entry(relative.clone()).or_default().push(raw);
            } else {
                misplaced_raw.push(format!("{relative}: raw('{raw}')"));
            }
        }
        if relative == RAW_MODULE || relative == "src/lib/copy/raw.ts" {
            // The one module whose whole purpose is to name internal terms deliberately,
            // and the function that marks them.
            continue;
        }

        let in_copy_dir = relative.starts_with("src/lib/copy/");
        let is_svelte = relative.ends_with(".svelte");
        if !in_copy_dir && !is_svelte {
            // Logic modules are code: `stream.components` is a field name, not a label.
            continue;
        }
        checked += 1;

        for (number, candidate) in user_facing(&text, is_svelte) {
            let lowered = candidate.to_lowercase();
            for term in BANNED {
                if contains_word(&lowered, term) {
                    hits.push(Hit {
                        file: relative.clone(),
                        line: number,
                        term: (*term).to_string(),
                        text: candidate.trim().chars().take(100).collect(),
                    });
                    break;
                }
            }
        }
    }

    println!(
        "The vocabulary lint (W5 §6.6, D-36). {checked} copy modules and templates \
         checked.\n"
    );
    println!("  the exception list, printed on every run so it cannot grow quietly");
    if exceptions.is_empty() {
        println!("    (none)");
    } else {
        for (file, terms) in &exceptions {
            println!("    {file}");
            for term in terms {
                println!("      raw('{term}')");
            }
        }
    }
    println!();

    if !misplaced_raw.is_empty() {
        eprintln!(
            "ui-copy: `raw()` is permitted only in {RAW_MODULE}, the one screen whose \
             audience is debugging.\n"
        );
        for m in &misplaced_raw {
            eprintln!("  - {m}");
        }
        return ExitCode::from(3);
    }

    if !hits.is_empty() {
        eprintln!("ui-copy: an internal term reached a user-facing string.\n");
        for hit in &hits {
            eprintln!("  {}:{}  `{}`", hit.file, hit.line, hit.term);
            eprintln!("    {}", hit.text);
        }
        eprintln!(
            "\nD-36: no internal term reaches the screen. The §0.2 table has the plain \
             word for each of these; if the term genuinely has to appear, it belongs in \
             {RAW_MODULE} as `raw('...')` and on the exception list above."
        );
        return ExitCode::from(1);
    }

    println!("ui-copy: no internal term reaches a user-facing string.");
    ExitCode::SUCCESS
}

/// Remove comments, so an explanation of the rule is not a use of it.
///
/// Two syntaxes because there are two kinds of file, and both matter: the copy modules
/// document the escape hatch in `/** */` blocks and the `.svelte` files document the
/// design in `<!-- -->` blocks. Neither is a string a person can read.
fn strip_comments(text: &str, is_svelte: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if is_svelte && chars[i] == '<' {
            let tail: String = chars[i..(i + 4).min(chars.len())].iter().collect();
            if tail.starts_with("<!--") {
                while i < chars.len() {
                    let close: String = chars[i..(i + 3).min(chars.len())].iter().collect();
                    if close.starts_with("-->") {
                        i += 3;
                        break;
                    }
                    if chars[i] == '\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
                continue;
            }
        }
        if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                if chars[i] == '\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(chars.len());
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Every `raw('x')` argument in a file.
fn raw_terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("raw(") {
        rest = &rest[at + 4..];
        let Some(quote) = rest.chars().next() else {
            break;
        };
        if quote != '\'' && quote != '"' && quote != '`' {
            continue;
        }
        let body = &rest[quote.len_utf8()..];
        let Some(end) = body.find(quote) else { break };
        out.push(body[..end].to_string());
        rest = &body[end..];
    }
    out
}

/// The strings a person can actually read, with their line numbers.
///
/// This is the whole difficulty of the lint, and getting it wrong in either direction
/// makes it useless: checking entire files flags the design-document comments that
/// legitimately name the component model -- which would make the gate's output noise a
/// reviewer learns to ignore -- and checking nothing but obvious labels misses the
/// attribute values a screen reader announces.
///
/// So: in a `.ts` copy module, **string literals** outside comments. In a `.svelte` file,
/// the template's **text nodes** and the handful of attributes whose values reach a person
/// -- `aria-label`, `title`, `placeholder`, `label`, `alt` -- with `<script>`, `<style>`
/// and HTML comments excluded.
fn user_facing(text: &str, is_svelte: bool) -> Vec<(usize, String)> {
    if is_svelte {
        template_strings(text)
    } else {
        string_literals(text)
    }
}

/// String literals in TypeScript, skipping `//` and `/* */` comments.
fn string_literals(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut line = 1usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                if chars[i] == '\n' {
                    line += 1;
                }
                i += 1;
            }
            i = (i + 2).min(chars.len());
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            let quote = c;
            let start_line = line;
            let mut literal = String::new();
            i += 1;
            while i < chars.len() && chars[i] != quote {
                if chars[i] == '\\' {
                    i += 2;
                    continue;
                }
                if chars[i] == '\n' {
                    line += 1;
                }
                // A template literal's `${...}` is **code**, not text. Skipping it is the
                // same rule a mustache gets, for the same reason: `${p.stable_device}` is
                // a field access and the only readable part of the literal is the prose
                // around it.
                if quote == '`' && chars[i] == '$' && chars.get(i + 1).is_some_and(|c| *c == '{') {
                    let mut depth = 0usize;
                    while i < chars.len() {
                        if chars[i] == '{' {
                            depth += 1;
                        } else if chars[i] == '}' {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        } else if chars[i] == '\n' {
                            line += 1;
                        }
                        i += 1;
                    }
                    // A space stands in for the interpolation, so two words either side of
                    // it do not become one.
                    literal.push(' ');
                    continue;
                }
                literal.push(chars[i]);
                i += 1;
            }
            i += 1;
            out.push((start_line, literal));
            continue;
        }
        i += 1;
    }
    out
}

/// The attributes whose values a person reads or hears.
const SPOKEN_ATTRIBUTES: &[&str] = &["aria-label", "title", "placeholder", "label", "alt"];

/// Template text nodes and spoken attribute values from a `.svelte` file.
fn template_strings(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut line = 1usize;
    let mut text_node = String::new();
    let mut node_line = 1usize;

    // Skipping `<script>` and `<style>` wholesale is right rather than lazy: a label
    // written in a script block reaches the template as an expression, and the
    // expression's *source* is the copy module this lint already checks.
    let skip_blocks = ["<script", "<style"];

    while i < chars.len() {
        if chars[i] == '\n' {
            line += 1;
            text_node.push('\n');
            i += 1;
            continue;
        }
        // A mustache is **code**, not text.
        //
        // `{#each stream.components as c}` and `{component.counters.runs_total}` are
        // expressions over field names; the strings they evaluate to come either from a
        // copy module, which this lint checks, or from the server, which renders the
        // vocabulary itself (§2.3). Treating a mustache as readable text made the first
        // version of this gate report twenty-odd field accesses as vocabulary violations,
        // which is exactly the noise that gets a lint switched off.
        //
        // String **literals inside** a mustache are still checked, because an inline
        // label in a template is precisely what §6.6 moves into the copy modules.
        if chars[i] == '{' {
            if !text_node.trim().is_empty() {
                out.push((node_line, std::mem::take(&mut text_node)));
            }
            text_node.clear();
            let start = i;
            let mut depth = 0usize;
            let mut quote: Option<char> = None;
            while i < chars.len() {
                let c = chars[i];
                if c == '\n' {
                    line += 1;
                }
                match quote {
                    Some(q) if c == q => quote = None,
                    Some(_) => {}
                    None if c == '\'' || c == '"' || c == '`' => quote = Some(c),
                    None if c == '{' => depth += 1,
                    None if c == '}' => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    None => {}
                }
                i += 1;
            }
            let body: String = chars[start..i.min(chars.len())].iter().collect();
            for (_, literal) in string_literals(&body) {
                out.push((node_line, literal));
            }
            node_line = line;
            continue;
        }
        if chars[i] == '<' {
            let tail: String = chars[i..(i + 8).min(chars.len())].iter().collect();
            // An HTML comment: the design rationale lives in these, and it names the
            // component model on purpose.
            if tail.starts_with("<!--") {
                if !text_node.trim().is_empty() {
                    out.push((node_line, std::mem::take(&mut text_node)));
                }
                text_node.clear();
                while i < chars.len() {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    let close: String = chars[i..(i + 3).min(chars.len())].iter().collect();
                    if close.starts_with("-->") {
                        i += 3;
                        break;
                    }
                    i += 1;
                }
                node_line = line;
                continue;
            }
            let lowered = tail.to_lowercase();
            if let Some(block) = skip_blocks.iter().find(|b| lowered.starts_with(*b)) {
                let end_tag = format!("</{}>", block.trim_start_matches('<'));
                let remaining: String = chars[i..].iter().collect();
                let span = remaining
                    .find(&end_tag)
                    .map_or(remaining.len(), |at| at + end_tag.len());
                line += remaining[..span].matches('\n').count();
                i += remaining[..span].chars().count();
                node_line = line;
                text_node.clear();
                continue;
            }

            // An ordinary tag. Flush the text before it, then read its attributes.
            if !text_node.trim().is_empty() {
                out.push((node_line, std::mem::take(&mut text_node)));
            }
            text_node.clear();
            let tag_start = i;
            let mut quote: Option<char> = None;
            // Brace depth, because an attribute's value is a mustache and a mustache can
            // contain `>`: `onValueChange={(v) => (device = v)}` ends the tag at the arrow
            // if the scanner only looks for `>`. That bug made the rest of the element's
            // source read as a text node, which is how a field assignment came to be
            // reported as a vocabulary violation.
            let mut braces = 0usize;
            while i < chars.len() {
                let c = chars[i];
                if c == '\n' {
                    line += 1;
                }
                match quote {
                    Some(q) if c == q => quote = None,
                    Some(_) => {}
                    None if c == '"' || c == '\'' || c == '`' => quote = Some(c),
                    None if c == '{' => braces += 1,
                    None if c == '}' => braces = braces.saturating_sub(1),
                    None if c == '>' && braces == 0 => {
                        i += 1;
                        break;
                    }
                    None => {}
                }
                i += 1;
            }
            let tag: String = chars[tag_start..i.min(chars.len())].iter().collect();
            for (attribute_line, value) in spoken_attributes(&tag, line) {
                out.push((attribute_line, value));
            }
            node_line = line;
            continue;
        }
        text_node.push(chars[i]);
        i += 1;
    }
    if !text_node.trim().is_empty() {
        out.push((node_line, text_node));
    }
    out
}

fn spoken_attributes(tag: &str, line: usize) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for name in SPOKEN_ATTRIBUTES {
        let needle = format!("{name}=\"");
        let mut rest = tag;
        while let Some(at) = rest.find(&needle) {
            rest = &rest[at + needle.len()..];
            let Some(end) = rest.find('"') else { break };
            out.push((line, rest[..end].to_string()));
            rest = &rest[end..];
        }
    }
    out
}

/// Whether `haystack` contains `term` as a whole word.
///
/// Word-bounded, because the alternative flags "components" inside "component" -- which is
/// correct -- and also "source" inside "resource", "outsourced" and "sources", which is
/// not. A lint whose output a reviewer learns to ignore is worse than no lint.
fn contains_word(haystack: &str, term: &str) -> bool {
    let mut from = 0;
    while let Some(at) = haystack[from..].find(term) {
        let start = from + at;
        let end = start + term.len();
        let before_ok = !haystack[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-');
        // A trailing `s` is the same word: "components" is "component".
        let after = &haystack[end..];
        let next = after.chars().next();
        let after_ok = match next {
            None => true,
            Some(c) if !(c.is_alphanumeric() || c == '_' || c == '-') => true,
            Some('s') => !after[1..].chars().next().is_some_and(char::is_alphanumeric),
            Some(_) => false,
        };
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

// =======================================================================================
// cargo xtask ui -- building the bundle
// =======================================================================================

/// Build the UI bundle with `pnpm`, natively.
///
/// **D-58**: the bundle is architecture-independent static files, so building it inside
/// each `cross` container would be strictly harder -- a Node toolchain per target, for
/// output that is byte-identical across all of them. So it is built **once, natively,
/// before** the cross matrix, and [`crate::dist`] fails rather than packaging a release
/// without it.
///
/// `--frozen-lockfile` so that a release build cannot silently resolve a different
/// dependency tree than the one that was reviewed.
pub fn build(root: &Path) -> Result<(), String> {
    let ui = root.join("ui");
    if !ui.join("package.json").is_file() {
        return Err(format!("there is no UI project at {}", ui.display()));
    }
    run(&ui, &["install", "--frozen-lockfile"])?;
    run(&ui, &["run", "check"])?;
    run(&ui, &["run", "lint"])?;
    run(&ui, &["run", "test"])?;
    run(&ui, &["run", "build"])?;
    println!(
        "ui: built {}",
        ui.join("build").strip_prefix(root).unwrap_or(&ui).display()
    );
    Ok(())
}

fn run(dir: &Path, args: &[&str]) -> Result<(), String> {
    let status = std::process::Command::new("pnpm")
        .args(args)
        .current_dir(dir)
        .status()
        .map_err(|e| {
            format!(
                "running `pnpm {}`: {e}. The UI needs pnpm and Node on PATH; the Rust \
                 build does not, which is why the bundle is built by its own command.",
                args.join(" ")
            )
        })?;
    if !status.success() {
        return Err(format!("`pnpm {}` failed with {status}", args.join(" ")));
    }
    Ok(())
}

/// Whether the committed bundle is newer than every source file it was built from.
///
/// **D-58's second half**: `cargo xtask dist` "fails if it is missing or stale". Staleness
/// is a timestamp comparison rather than a content hash, and the choice is deliberate --
/// a hash would need a manifest committed beside the bundle, which is a second artefact
/// to keep in step, and the failure mode a release cares about is "somebody edited the UI
/// and did not rebuild", which a timestamp catches.
pub fn stale(root: &Path) -> Result<Option<PathBuf>, String> {
    let ui = root.join("ui");
    let built = ui.join("build/index.html");
    let built_at = std::fs::metadata(&built)
        .and_then(|m| m.modified())
        .map_err(|_| format!("there is no bundle at {}", ui.join("build").display()))?;
    let mut sources = Vec::new();
    walk(&ui.join("src"), &mut sources);
    sources.push(ui.join("package.json"));
    sources.push(ui.join("svelte.config.js"));
    sources.push(ui.join("vite.config.ts"));
    for source in sources {
        let Ok(modified) = std::fs::metadata(&source).and_then(|m| m.modified()) else {
            continue;
        };
        if modified > built_at {
            return Ok(Some(source));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_banned_word_is_matched_as_a_whole_word() {
        assert!(contains_word("the component is running", "component"));
        assert!(contains_word("two components restarted", "component"));
        // The three that made a naive substring match useless.
        assert!(!contains_word("one resource", "source"));
        assert!(!contains_word("outsourced work", "source"));
        assert!(!contains_word("componentry", "component"));
    }

    #[test]
    fn a_typescript_comment_is_not_a_user_facing_string() {
        let text = "// the component model says so\nconst a = 'Running.';\n";
        let found = string_literals(text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "Running.");
    }

    #[test]
    fn a_svelte_html_comment_is_not_a_user_facing_string() {
        let text = "<!-- §2.3's component list -->\n<p>Running.</p>\n";
        let found = template_strings(text);
        let joined: String = found.iter().map(|(_, s)| s.as_str()).collect();
        assert!(joined.contains("Running."));
        assert!(!joined.to_lowercase().contains("component"));
    }

    #[test]
    fn a_svelte_script_block_is_not_a_user_facing_string() {
        let text = "<script>\n  const spool = 1; // envelope\n</script>\n<p>Unsent readings</p>\n";
        let found = template_strings(text);
        let joined: String = found.iter().map(|(_, s)| s.as_str()).collect();
        assert!(joined.contains("Unsent readings"));
        assert!(!joined.to_lowercase().contains("spool"));
    }

    /// A mustache is code and is not checked for field names, but a string literal
    /// written inside one is -- that is an inline label, which is the thing §6.6 moves
    /// into the copy modules.
    #[test]
    fn a_mustache_is_code_and_its_literals_are_still_text() {
        let text = "<p>{#each stream.components as c}{c.display.name}{/each}</p>\n";
        let found = template_strings(text);
        let joined: String = found.iter().map(|(_, s)| s.as_str()).collect();
        assert!(
            !joined.to_lowercase().contains("component"),
            "a field access is not a label: {joined:?}"
        );

        let inline = "<p>{a ? 'The component restarted' : 'ok'}</p>\n";
        let found = template_strings(inline);
        let joined: String = found.iter().map(|(_, s)| s.as_str()).collect();
        assert!(joined.contains("The component restarted"));
    }

    /// A template literal's interpolation is code, and the prose around it is not.
    #[test]
    fn a_template_interpolation_is_code() {
        let found = string_literals("const a = `${p.stable_device} — ${p.description}`;");
        assert_eq!(found.len(), 1);
        assert!(!found[0].1.to_lowercase().contains("device"));
        assert!(found[0].1.contains('—'));
    }

    /// An arrow function in an attribute does not end the tag.
    #[test]
    fn an_arrow_function_in_an_attribute_does_not_end_the_tag() {
        let text = "<Select onValueChange={(v) => (device = v)} />\n<p>Serial port</p>\n";
        let found = template_strings(text);
        let joined: String = found.iter().map(|(_, s)| s.as_str()).collect();
        assert!(joined.contains("Serial port"));
        assert!(
            !joined.to_lowercase().contains("device"),
            "the assignment leaked into a text node: {joined:?}"
        );
    }

    /// A spoken attribute is checked, because a screen reader reads it out.
    #[test]
    fn an_aria_label_is_a_user_facing_string() {
        let text = "<button aria-label=\"Restart the component\">Go</button>\n";
        let found = template_strings(text);
        let joined: String = found.iter().map(|(_, s)| s.as_str()).collect();
        assert!(joined.contains("Restart the component"));
    }

    #[test]
    fn the_escape_hatch_is_extracted_with_its_term() {
        let terms = raw_terms("`A ${raw('component')} id looks like ${raw('wu:backyard')}`");
        assert_eq!(terms, vec!["component", "wu:backyard"]);
    }

    /// Documenting the escape hatch must not be a use of it. Both comment syntaxes,
    /// because the first version of this gate failed on its own documentation in three
    /// files -- which is the shape of mistake that makes a team turn a lint off.
    #[test]
    fn an_explanation_of_the_escape_hatch_is_not_a_use_of_it() {
        let ts = "/** The hatch is written raw('ReadingStore'). */\nconst a = 'Running.';\n";
        assert!(raw_terms(&strip_comments(ts, false)).is_empty());
        let svelte = "<!-- every term is raw('Capability') here -->\n<p>Running.</p>\n";
        assert!(raw_terms(&strip_comments(svelte, true)).is_empty());
        // And a real use still is one.
        let real = "const t = `${raw('Spool')}`;\n";
        assert_eq!(raw_terms(&strip_comments(real, false)), vec!["Spool"]);
    }

    /// The initial route is what `index.html` tells the browser to fetch, which is the
    /// thing §6.5's first row is about.
    #[test]
    fn the_initial_route_comes_from_the_document() {
        let html = "<head><link href=\"/assets/a.js\" rel=\"modulepreload\">\
                    <link href=\"/assets/b.css\" rel=\"stylesheet\">\
                    <link href=\"/favicon.svg\"></head><body></body>";
        let paths = initial_route_paths(html);
        assert_eq!(
            paths.into_iter().collect::<Vec<_>>(),
            vec!["assets/a.js".to_string(), "assets/b.css".to_string()]
        );
    }
}
