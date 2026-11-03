//! The command line, and the one place a message may reach a terminal directly.
//!
//! `ci/layers.toml`'s `no-stdout-logging` rule exempts this file by path. §5.1 allows
//! `println!` and `eprintln!` "only on the binary's argument-parsing path, which is the
//! one place a message has to reach a terminal before a subscriber exists" -- and that
//! rule used to be enforced at crate granularity with a note saying the narrowing arrives
//! with this work item. It has: the rule now names this file, so `println!` anywhere else
//! in the binary is a gate failure rather than a convention.
//!
//! **Hand-rolled rather than `clap`.** Four options, one of them required, and DESIGN.md
//! D-32's table is the list of crates this program may use for an idiom it needs -- "a
//! substitution is an amendment, not a manifest edit". An argument parser is not in it,
//! and `--config <path>` is W8 assumption 2's entire requirement: the systemd unit and the
//! Dockerfile both spell it that way, so what matters is that the spelling is exact.

use std::path::PathBuf;

use mw_obs::Requested;

/// The parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// `--config <path>`: the SQLite configuration database. W8's unit passes
    /// `/var/lib/modernweather/config.db`.
    pub config: PathBuf,
    /// `--log-format <pretty|json>`, which beats the detection of §5.3.
    pub format: Option<Requested>,
    /// `--log-level <directive>`, in `RUST_LOG` syntax. Overrides `RUST_LOG` itself,
    /// because an explicit flag is more specific than an inherited environment.
    pub directive: Option<String>,
}

/// What a parse produced.
#[derive(Debug)]
pub enum Parsed {
    /// Run the daemon.
    Run(Args),
    /// `--help` or `--version` was asked for and has been printed.
    Printed,
}

/// Why a command line was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArgsError {
    #[error("--config <path> is required")]
    NoConfig,
    #[error("{flag} needs a value")]
    MissingValue {
        /// The flag that was last on the line.
        flag: String,
    },
    #[error("`{value}` is not a log format; expected one of {expected}")]
    BadFormat {
        /// What was given.
        value: String,
        /// The accepted spellings.
        expected: &'static str,
    },
    #[error("unrecognised argument `{arg}`")]
    Unknown {
        /// What was given.
        arg: String,
    },
}

const USAGE: &str = "\
modernweather - weather station data collection and distribution

Usage: modernweather --config <path> [options]

Options:
  --config <path>          The SQLite configuration database. Created if absent;
                           migrations are applied on every start.
  --log-format <format>    pretty | json. Default: pretty on a terminal, JSON otherwise.
  --log-level <directive>  RUST_LOG syntax, e.g. `info,mw_station=debug`. Overrides
                           RUST_LOG. Adjustable at run time through the management API.
  -h, --help               Print this and exit.
  -V, --version            Print the version and exit.
";

/// Parse the arguments, with `--help` and `--version` handled here.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed, ArgsError> {
    let mut config = None;
    let mut format = None;
    let mut directive = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        // `--flag=value` and `--flag value` both, because the systemd unit writes the
        // second and a Dockerfile `CMD` array often writes the first.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = || match inline.clone() {
            Some(v) => Ok(v),
            None => args
                .next()
                .ok_or_else(|| ArgsError::MissingValue { flag: flag.clone() }),
        };
        match flag.as_str() {
            "--config" => config = Some(PathBuf::from(value()?)),
            "--log-format" => {
                let v = value()?;
                format = Some(Requested::parse(&v).ok_or(ArgsError::BadFormat {
                    value: v,
                    expected: Requested::SPELLINGS,
                })?);
            }
            "--log-level" => directive = Some(value()?),
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(Parsed::Printed);
            }
            "-V" | "--version" => {
                println!("modernweather {}", env!("CARGO_PKG_VERSION"));
                return Ok(Parsed::Printed);
            }
            _ => return Err(ArgsError::Unknown { arg }),
        }
    }
    Ok(Parsed::Run(Args {
        config: config.ok_or(ArgsError::NoConfig)?,
        format,
        directive,
    }))
}

/// Report a failure that happened before -- or instead of -- the subscriber.
///
/// The only other caller is the boot sequence's own failure path: if installing the
/// subscriber is what failed, `error!` has nowhere to go, and a daemon that exits silently
/// is the thing systemd's `Type=notify` was adopted to stop happening.
pub fn fatal(message: &str) {
    eprintln!("modernweather: {message}");
}

/// Print the usage after a rejected command line.
pub fn usage() {
    eprint!("{USAGE}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> Args {
        match parse(args.iter().map(|s| (*s).to_string())) {
            Ok(Parsed::Run(a)) => a,
            other => panic!("expected a runnable command line, got {other:?}"),
        }
    }

    fn parse_err(args: &[&str]) -> ArgsError {
        match parse(args.iter().map(|s| (*s).to_string())) {
            Err(e) => e,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn the_spelling_w8_assumption_2_requires_is_the_one_that_works() {
        // The exact string in packaging/systemd/modernweather.service.
        let args = parse_ok(&["--config", "/var/lib/modernweather/config.db"]);
        assert_eq!(
            args.config,
            PathBuf::from("/var/lib/modernweather/config.db")
        );
        assert_eq!(args.format, None);
        assert_eq!(args.directive, None);
    }

    #[test]
    fn the_unit_file_still_passes_what_this_parser_accepts() {
        // Read rather than quoted: the unit is the contract, and a flag renamed here
        // while the unit kept the old spelling is a daemon that will not start on the
        // machine it ships to.
        let unit = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../packaging/systemd/modernweather.service"),
        )
        .expect("the unit file is in the tree");
        let exec = unit
            .lines()
            .find_map(|l| l.trim().strip_prefix("ExecStart="))
            .expect("the unit has an ExecStart");
        let argv: Vec<String> = exec
            .split_whitespace()
            .skip(1)
            .map(str::to_string)
            .collect();
        match parse(argv) {
            Ok(Parsed::Run(a)) => {
                assert_eq!(a.config, PathBuf::from("/var/lib/modernweather/config.db"))
            }
            other => panic!("the shipped unit's ExecStart is not accepted: {other:?}"),
        }
    }

    #[test]
    fn both_spellings_of_a_value_are_accepted() {
        assert_eq!(
            parse_ok(&["--config=/tmp/a.db"]).config,
            PathBuf::from("/tmp/a.db")
        );
        assert_eq!(
            parse_ok(&["--config", "/tmp/a.db"]).config,
            PathBuf::from("/tmp/a.db")
        );
    }

    #[test]
    fn the_log_options_are_parsed_and_a_bad_format_names_what_is_accepted() {
        let args = parse_ok(&[
            "--config",
            "/tmp/a.db",
            "--log-format",
            "json",
            "--log-level",
            "info,mw_station=debug",
        ]);
        assert_eq!(args.format, Some(Requested::Json));
        assert_eq!(args.directive.as_deref(), Some("info,mw_station=debug"));

        let e = parse_err(&["--config", "/tmp/a.db", "--log-format", "verbose"]);
        assert_eq!(
            e,
            ArgsError::BadFormat {
                value: "verbose".into(),
                expected: Requested::SPELLINGS
            }
        );
        assert!(e.to_string().contains("pretty, json"), "{e}");
    }

    #[test]
    fn a_missing_configuration_is_refused_rather_than_defaulted() {
        // Deliberately not defaulted to /var/lib/modernweather/config.db: a daemon that
        // invents a path creates an empty database somewhere nobody looked, and then
        // reports that it has no stations.
        assert_eq!(parse_err(&[]), ArgsError::NoConfig);
    }

    #[test]
    fn a_flag_with_no_value_and_an_unknown_flag_are_both_rejected() {
        assert_eq!(
            parse_err(&["--config"]),
            ArgsError::MissingValue {
                flag: "--config".into()
            }
        );
        assert_eq!(
            parse_err(&["--config", "/tmp/a.db", "--verbose"]),
            ArgsError::Unknown {
                arg: "--verbose".into()
            }
        );
    }

    #[test]
    fn help_and_version_print_and_do_not_run() {
        for flag in ["-h", "--help", "-V", "--version"] {
            assert!(
                matches!(parse([flag.to_string()]), Ok(Parsed::Printed)),
                "{flag} should print and stop"
            );
        }
    }
}
