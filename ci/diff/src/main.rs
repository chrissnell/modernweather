//! `mw-diff` -- the M5-2 diff harness. `cargo run -- help` is the usage.
//!
//! Exit codes, because they are what a script reads:
//!   0  every difference was predicted by `expected.toml`
//!   1  at least one was not. This is a result, not a malfunction
//!   2  the run could not happen: usage, a connection, a malformed expected set

use std::process::ExitCode;

use mw_diff::census::{Census, Censuses};
use mw_diff::clock::{format_utc_ms, now_ms, parse_duration_secs, parse_utc_ms};
use mw_diff::expected::Expected;
use mw_diff::inject::{Injection, Side as InjectSide};
use mw_diff::report::{self, Provenance};
use mw_diff::source::{HttpSide, RowSide};
use mw_diff::{readings, responses};

/// The committed expected-diff set, compiled in.
///
/// Embedded rather than read from a path, so that the binary cannot be run against a
/// different rule set by accident and a copied binary still works. `--expected <path>` reads
/// one from disk, which is for iterating on a rule, and the report says which one it used.
const EMBEDDED_EXPECTED: &str = include_str!("../expected.toml");

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("mw-diff: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    match cmd {
        "readings" => mechanism_b(&args[1..]),
        "responses" => mechanism_c(&args[1..]),
        "help" | "--help" | "-h" => {
            print!("{}", usage());
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("no such mechanism `{other}`. Run `mw-diff help`")),
    }
}

fn usage() -> String {
    "\
mw-diff <mechanism> [options]

  readings    (b) same station, both daemons, diffed readings
  responses   (c) same database, both read paths, diffed responses

(b) readings
  --go <psql:CMD|tsv:PATH>      the Go daemon's database, or a recorded dump
  --rust <psql:CMD|tsv:PATH>    the new daemon's database, or a recorded dump
  --go-station <name>           weather.stationname, e.g. CSI
  --rust-station <id>           observations.station_id, e.g. csi
  --last <90s|30m|24h|7d>       the window, ending now (default 1h)
  --since <UTC> --until <UTC>   the window, explicitly. YYYY-MM-DDTHH:MM:SSZ
  --verbose                     one line per metric, including the equal ones

(c) responses
  --go <BASE_URL|dir:PATH>      the Go daemon's base URL, or a recorded corpus
  --rust <BASE_URL|dir:PATH>    the new daemon's base URL, or a recorded corpus
  --station <name>              the `station` query parameter, e.g. CSI
  --census <psql:CMD|tsv:PATH>  readings per bucket, from the GO side. Without it
                                DEV-4's and DEV-7's bounds have no n and every
                                bucket they apply to is reported as no_census
  --census-rust <psql:|tsv:>    the same from the new side, which is what lets an
                                unevenly populated bucket be told from a value
                                difference. Needs --rust-station
  --rust-station <id>           observations.station_id, for --census-rust

both
  --expected <PATH>             an expected-diff set from disk instead of the
                                committed one compiled into this binary
  --inject <KEY:PCT[:go|rust]>  the negative control: scale one metric by PCT%
                                on one side and require the harness to report it

environment
  MW_DIFF_GO, MW_DIFF_RUST, MW_DIFF_CENSUS, MW_DIFF_CENSUS_RUST
                                defaults for the matching options

exit  0 every difference was predicted   1 at least one was not   2 could not run
"
    .to_string()
}

/// A flag parser that refuses what it does not know.
///
/// A harness that ignored an unknown flag would run the wrong comparison and report a
/// verdict about it -- `--rust-stations` for `--rust-station` is a window against the wrong
/// station, not a typo somebody notices.
struct Flags {
    map: std::collections::BTreeMap<String, String>,
}

impl Flags {
    fn parse(args: &[String], known: &[&str], switches: &[&str]) -> Result<Flags, String> {
        let mut map = std::collections::BTreeMap::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(name) = a.strip_prefix("--") {
                if switches.contains(&name) {
                    map.insert(name.to_string(), "yes".into());
                    i += 1;
                    continue;
                }
                if !known.contains(&name) {
                    return Err(format!("unknown option `--{name}`. Run `mw-diff help`"));
                }
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| format!("--{name} needs a value"))?;
                map.insert(name.to_string(), value.clone());
                i += 2;
            } else {
                return Err(format!("unexpected argument `{a}`. Run `mw-diff help`"));
            }
        }
        Ok(Flags { map })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.map.get(name).map(String::as_str)
    }

    fn env_or(&self, name: &str, var: &str) -> Option<String> {
        self.get(name)
            .map(str::to_string)
            .or_else(|| std::env::var(var).ok())
    }

    fn need(&self, name: &str) -> Result<&str, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required. Run `mw-diff help`"))
    }

    fn on(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }
}

fn load_expected(flags: &Flags) -> Result<(Expected, String), String> {
    match flags.get("expected") {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            Ok((Expected::parse(&text)?, format!("{path} (from disk)")))
        }
        None => Ok((
            Expected::parse(EMBEDDED_EXPECTED)?,
            "ci/diff/expected.toml (compiled in)".to_string(),
        )),
    }
}

fn mechanism_b(args: &[String]) -> Result<ExitCode, String> {
    let flags = Flags::parse(
        args,
        &[
            "go",
            "rust",
            "go-station",
            "rust-station",
            "last",
            "since",
            "until",
            "expected",
            "inject",
        ],
        &["verbose"],
    )?;
    let (expected, expected_from) = load_expected(&flags)?;

    let go_spec = flags
        .env_or("go", "MW_DIFF_GO")
        .ok_or("--go is required, or set MW_DIFF_GO")?;
    let rust_spec = flags
        .env_or("rust", "MW_DIFF_RUST")
        .ok_or("--rust is required, or set MW_DIFF_RUST")?;
    let go_side = RowSide::parse(&go_spec)?;
    let rust_side = RowSide::parse(&rust_spec)?;
    let go_station = flags.need("go-station")?;
    let rust_station = flags.need("rust-station")?;

    let (from_ms, until_ms) = match (flags.get("since"), flags.get("until"), flags.get("last")) {
        (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => {
            return Err(
                "--last and --since/--until are two ways to say the same thing. \
                        Give one"
                    .into(),
            )
        }
        (Some(s), until, None) => {
            let from = parse_utc_ms(s)?;
            let to = match until {
                Some(u) => parse_utc_ms(u)?,
                None => now_ms(),
            };
            (from, to)
        }
        (None, Some(_), None) => return Err("--until without --since".into()),
        (None, None, last) => {
            let secs = parse_duration_secs(last.unwrap_or("1h"))?;
            let now = now_ms();
            (now - secs * 1_000, now)
        }
    };
    if until_ms <= from_ms {
        return Err("the window ends before it starts".into());
    }

    let injection = flags.get("inject").map(Injection::parse).transpose()?;

    // Loaded wider than the window by the pairing tolerance, and compared inside it: a frame
    // whose two receipt stamps straddle an edge must have both halves available or it becomes
    // an unpaired reading for no reason but the edge.
    let slack = expected.pair_tolerance_ms;
    let go = readings::load(
        &go_side,
        readings::GO,
        "go",
        go_station,
        from_ms - slack,
        until_ms + slack,
        &expected,
    )?;
    let rust = readings::load(
        &rust_side,
        readings::RUST,
        "rust",
        rust_station,
        from_ms - slack,
        until_ms + slack,
        &expected,
    )?;

    let (mut go, mut rust) = (go, rust);
    let mut injected_note = None;
    if let Some(inj) = &injection {
        let touched = match inj.side {
            InjectSide::Go => inj.apply_rows(&go.columns.clone(), &mut go.rows),
            InjectSide::Rust => inj.apply_rows(&rust.columns.clone(), &mut rust.rows),
        };
        if touched == 0 {
            // Not a result: a control that changed nothing would report a clean run and be
            // read as evidence that the harness works.
            return Err(format!(
                "--inject {}:{} moved no value. Either the column is absent from the window \
                 or it is zero throughout it, and an injection that moves nothing is not a \
                 control",
                inj.key, inj.percent
            ));
        }
        injected_note = Some(format!("{} -- {touched} value(s) moved", inj.describe()));
    }

    let mut lines = vec![
        (
            "go".to_string(),
            format!("weather.stationname = '{go_station}'   {}", go.provenance),
        ),
        (
            "rust".to_string(),
            format!(
                "observations.station_id = '{rust_station}'   {}",
                rust.provenance
            ),
        ),
        (
            "window".to_string(),
            format!(
                "{} .. {}   pair tolerance {} ms",
                format_utc_ms(from_ms),
                format_utc_ms(until_ms),
                expected.pair_tolerance_ms
            ),
        ),
        ("expected".to_string(), expected_from),
    ];
    if let Some(note) = injected_note {
        lines.push(("control".to_string(), note));
    }

    let outcome = readings::diff(go, rust, &expected, (from_ms, until_ms));
    print!(
        "{}",
        report::readings(&outcome, &Provenance { lines }, flags.on("verbose"))
    );
    Ok(if outcome.is_green() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn mechanism_c(args: &[String]) -> Result<ExitCode, String> {
    let flags = Flags::parse(
        args,
        &[
            "go",
            "rust",
            "station",
            "census",
            "census-rust",
            "rust-station",
            "expected",
            "inject",
        ],
        &["verbose"],
    )?;
    let (expected, expected_from) = load_expected(&flags)?;

    let go_spec = flags
        .env_or("go", "MW_DIFF_GO")
        .ok_or("--go is required, or set MW_DIFF_GO")?;
    let rust_spec = flags
        .env_or("rust", "MW_DIFF_RUST")
        .ok_or("--rust is required, or set MW_DIFF_RUST")?;
    let go = HttpSide::parse(&go_spec)?;
    let rust = HttpSide::parse(&rust_spec)?;
    let station = flags.need("station")?;
    let injection = flags.get("inject").map(Injection::parse).transpose()?;

    // The Rust side's census counts `observations`, which is keyed by the station's slug and
    // not by its name -- so it needs `--rust-station` for the same reason mechanism (b) does.
    let rust_station = flags.get("rust-station");
    let one_census = |spec: Option<String>,
                      label: &str,
                      table: &'static str,
                      ident: &'static str,
                      who: Option<&str>|
     -> Result<Census, String> {
        let Some(spec) = spec else {
            return Ok(Census::default());
        };
        match spec.split_once(':') {
            Some(("tsv", p)) => Census::from_tsv(std::path::Path::new(p)),
            Some(("psql", c)) => {
                let name = who.ok_or_else(|| {
                    format!("--census-rust needs --rust-station, the `station_id` in {table}")
                })?;
                let psql = mw_diff::psql::Psql::new(label, c)?;
                Census::from_psql(&psql, table, ident, name, &expected.bands, now_ms())
            }
            _ => Err(format!("{spec}: expected `psql:<command>` or `tsv:<path>`")),
        }
    };
    let census = Censuses {
        go: one_census(
            flags.env_or("census", "MW_DIFF_CENSUS"),
            "census go",
            "weather",
            "stationname",
            Some(station),
        )?,
        rust: one_census(
            flags.env_or("census-rust", "MW_DIFF_CENSUS_RUST"),
            "census rust",
            "observations",
            "station_id",
            rust_station,
        )?,
    };

    let mut lines = vec![
        ("go".to_string(), go.describe()),
        ("rust".to_string(), rust.describe()),
        ("station".to_string(), station.to_string()),
        ("census".to_string(), census.describe()),
        ("expected".to_string(), expected_from),
    ];
    if let Some(inj) = &injection {
        lines.push(("control".to_string(), inj.describe()));
    }

    let mut outs = Vec::new();
    for req in responses::requests(&expected, station) {
        outs.push(responses::diff_endpoint(
            &req,
            &go,
            &rust,
            &expected,
            &census,
            injection.as_ref(),
        )?);
    }
    if let Some(inj) = &injection {
        let moved: usize = outs.iter().map(|o| o.injected).sum();
        if moved == 0 {
            return Err(format!(
                "--inject {}:{} moved no value across any endpoint. Either the key is absent \
                 or it is zero in every element, and an injection that moves nothing is not \
                 a control",
                inj.key, inj.percent
            ));
        }
    }
    print!("{}", report::responses(&outs, &Provenance { lines }));
    Ok(if outs.iter().all(|o| o.is_green()) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}
