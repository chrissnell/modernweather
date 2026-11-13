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

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
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

/// A `--migrate-config-from` command line.
///
/// Its own variant rather than two more fields on [`Args`], because it is a different thing
/// to do: nothing binds, nothing is planned and the process exits when the migration is
/// written. [`MigrateArgs::args`] is the part the two share -- where the configuration
/// database is, and how to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateArgs {
    /// `--config <path>` (the destination), `--log-format` and `--log-level`.
    pub args: Args,
    /// `--migrate-config-from <path>`: the Go daemon's own configuration database. **Read
    /// only**, and never written to.
    pub from: PathBuf,
    /// `--migrate-config-dry-run`: apply everything inside a transaction, print the report,
    /// and roll back. The same code path as a real migration, differing only in whether the
    /// transaction commits -- which is how `POST /api/v1/config/preview` and a save already
    /// relate (W5 §2.4).
    pub dry_run: bool,
}

/// What a parse produced.
#[derive(Debug)]
pub enum Parsed {
    /// Run the daemon.
    Run(Args),
    /// Translate a Go configuration database into the `--config` one, and exit.
    MigrateConfig(MigrateArgs),
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
    #[error(
        "--migrate-config-dry-run only means something with --migrate-config-from <path>, \
         and without it this would have started the daemon"
    )]
    DryRunWithoutSource,
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

Migrating from RemoteWeather (the Go daemon):
  --migrate-config-from <path>
                           Translate that daemon's own SQLite configuration database into
                           the one --config names, then exit without serving. The source is
                           read only. --config must not already hold configuration, and the
                           spool is placed beside it. Every value that was changed, that has
                           no successor here, or that you have to enter yourself is reported
                           one line at a time.
  --migrate-config-dry-run
                           Do all of that inside a transaction and roll it back, so the
                           report is produced and nothing is kept. Only with
                           --migrate-config-from.
";

/// Parse the arguments, with `--help` and `--version` handled here.
///
/// Over `OsString` rather than `String`, because the one argument that matters is a
/// **path**: a filename on Linux is bytes, not UTF-8, and `std::env::args()` panics mid
/// iteration on one that is not valid Unicode. A `--config` the daemon cannot spell is a
/// usage error with an exit code, not an abort.
pub fn parse<I: IntoIterator<Item = OsString>>(args: I) -> Result<Parsed, ArgsError> {
    let mut config = None;
    let mut format = None;
    let mut directive = None;
    let mut migrate_from: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        // `--flag=value` and `--flag value` both, because the systemd unit writes the
        // second and a Dockerfile `CMD` array often writes the first. Split on the `=`
        // BYTE so that a value which is not UTF-8 survives the split.
        let bytes = arg.as_bytes();
        let (flag, inline) = match bytes.iter().position(|b| *b == b'=') {
            Some(at) if bytes.starts_with(b"--") => (
                String::from_utf8_lossy(&bytes[..at]).into_owned(),
                Some(OsStr::from_bytes(&bytes[at + 1..]).to_os_string()),
            ),
            _ => (arg.to_string_lossy().into_owned(), None),
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
                // Lossy here and not for `--config`: a format is one of two ASCII words,
                // so anything that is not valid UTF-8 is a rejection either way, and the
                // message should show what was typed.
                let v = value()?.to_string_lossy().into_owned();
                format = Some(Requested::parse(&v).ok_or(ArgsError::BadFormat {
                    value: v,
                    expected: Requested::SPELLINGS,
                })?);
            }
            "--log-level" => directive = Some(value()?.to_string_lossy().into_owned()),
            // A path, like `--config`, so it is kept as bytes for the same reason.
            "--migrate-config-from" => migrate_from = Some(PathBuf::from(value()?)),
            "--migrate-config-dry-run" => dry_run = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(Parsed::Printed);
            }
            "-V" | "--version" => {
                println!("modernweather {}", env!("CARGO_PKG_VERSION"));
                return Ok(Parsed::Printed);
            }
            _ => {
                return Err(ArgsError::Unknown {
                    arg: arg.to_string_lossy().into_owned(),
                })
            }
        }
    }
    let args = Args {
        config: config.ok_or(ArgsError::NoConfig)?,
        format,
        directive,
    };
    match (migrate_from, dry_run) {
        (Some(from), dry_run) => Ok(Parsed::MigrateConfig(MigrateArgs {
            args,
            from,
            dry_run,
        })),
        // A dry run of nothing is a command line that asks for a migration and does not say
        // from where, and the daemon would otherwise start and serve -- which is the one
        // outcome somebody typing this flag did not want.
        (None, true) => Err(ArgsError::DryRunWithoutSource),
        (None, false) => Ok(Parsed::Run(args)),
    }
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
        match parse(args.iter().map(OsString::from)) {
            Ok(Parsed::Run(a)) => a,
            other => panic!("expected a runnable command line, got {other:?}"),
        }
    }

    fn parse_err(args: &[&str]) -> ArgsError {
        match parse(args.iter().map(OsString::from)) {
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
        let argv: Vec<OsString> = exec
            .split_whitespace()
            .skip(1)
            .map(OsString::from)
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

    /// A filename on Linux is bytes. `std::env::args()` panics mid iteration on one that
    /// is not valid Unicode, which would turn a mistyped path into an abort instead of an
    /// exit code -- and the path is the one argument this daemon cannot do without.
    #[test]
    fn a_configuration_path_that_is_not_utf8_is_still_a_path() {
        let invalid = OsStr::from_bytes(b"/var/lib/modernweather/\xff\xfe.db").to_os_string();
        for argv in [
            vec![OsString::from("--config"), invalid.clone()],
            vec![{
                let mut inline = OsString::from("--config=");
                inline.push(&invalid);
                inline
            }],
        ] {
            match parse(argv.clone()) {
                Ok(Parsed::Run(args)) => assert_eq!(
                    args.config.as_os_str().as_bytes(),
                    invalid.as_bytes(),
                    "the bytes reach the path verbatim"
                ),
                other => panic!("{argv:?} should parse, got {other:?}"),
            }
        }
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
                matches!(parse([OsString::from(flag)]), Ok(Parsed::Printed)),
                "{flag} should print and stop"
            );
        }
    }

    /// The two flags M7-4 adds, in both spellings, and the one combination that is a
    /// rejection rather than a default.
    #[test]
    fn a_configuration_migration_is_its_own_command_line() {
        let m = match parse(
            [
                "--config",
                "/var/lib/modernweather/config.db",
                "--migrate-config-from",
                "/etc/remoteweather/config.db",
            ]
            .iter()
            .map(OsString::from),
        ) {
            Ok(Parsed::MigrateConfig(m)) => m,
            other => panic!("expected a migration command line, got {other:?}"),
        };
        assert_eq!(
            m.args.config,
            PathBuf::from("/var/lib/modernweather/config.db")
        );
        assert_eq!(m.from, PathBuf::from("/etc/remoteweather/config.db"));
        assert!(!m.dry_run);

        // `--flag=value`, which a Dockerfile `CMD` array often writes.
        let m = match parse(
            [
                "--config=/d/config.db",
                "--migrate-config-from=/s/go.db",
                "--migrate-config-dry-run",
            ]
            .iter()
            .map(OsString::from),
        ) {
            Ok(Parsed::MigrateConfig(m)) => m,
            other => panic!("expected a migration command line, got {other:?}"),
        };
        assert_eq!(m.from, PathBuf::from("/s/go.db"));
        assert!(m.dry_run);

        // Without the source this would have STARTED THE DAEMON, which is the one outcome
        // somebody typing a dry-run flag did not want.
        let e = match parse(
            ["--config", "/d/config.db", "--migrate-config-dry-run"]
                .iter()
                .map(OsString::from),
        ) {
            Err(e) => e,
            other => panic!("expected a rejection, got {other:?}"),
        };
        assert_eq!(e, ArgsError::DryRunWithoutSource);

        // And the ordinary command line is untouched.
        assert!(matches!(
            parse(["--config", "/d/config.db"].iter().map(OsString::from)),
            Ok(Parsed::Run(_))
        ));
    }

    /// The usage text names both flags, because an operator at cutover reads `--help` and
    /// not this file.
    #[test]
    fn the_usage_names_the_migration() {
        assert!(USAGE.contains("--migrate-config-from"));
        assert!(USAGE.contains("--migrate-config-dry-run"));
    }
}
