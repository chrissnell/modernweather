//! The command line, and the one place a message may reach a terminal directly.
//!
//! `ci/layers.toml`'s `no-stdout-logging` rule exempts this file by path. §5.1 allows
//! `println!` and `eprintln!` "only on the binary's argument-parsing path, which is the
//! one place a message has to reach a terminal before a subscriber exists" -- and that
//! rule used to be enforced at crate granularity with a note saying the narrowing arrives
//! with this work item. It has: the rule now names this file, so `println!` anywhere else
//! in the binary is a gate failure rather than a convention.
//!
//! **Hand-rolled rather than `clap`.** A handful of options, one of them required, and
//! DESIGN.md D-32's table is the list of crates this program may use for an idiom it needs
//! -- "a substitution is an amendment, not a manifest edit". An argument parser is not in
//! it, and `--config <path>` is W8 assumption 2's entire requirement: the systemd unit and
//! the Dockerfile both spell it that way, so what matters is that the spelling is exact.
//!
//! **The three subcommands of W5 §3.6** arrived with implementation-plan.md **M7-2** and are
//! the reason there is a [`Command`] at all: a credential store with no backdoor needs a
//! recovery path, and the right trust boundary for it is local filesystem access. If you can
//! run `modernweather reset-password`, you can already read the configuration database --
//! which holds the weather database's password, the Weather Underground password and the
//! APRS passcode. A recovery path weaker than the thing it protects would be a hole; one
//! that is stronger would be theatre.
//!
//! Each of them takes `--config <path>`, which W5 §3.6 does not write. It is required for
//! the daemon's own reason, stated at `a_missing_configuration_is_refused_rather_than_defaulted`
//! below: a program that invents a path creates a database somewhere nobody looked and then
//! reports that it has no administrator.

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
    /// `--setup-listen <address>`, W5 §3.4's explicit opt-out for a headless install.
    ///
    /// The management API binds loopback until an administrator account exists. This widens
    /// that window deliberately, which is the property §3.4 asks for: it logs a `WARN`
    /// naming the risk, and it is **ignored once the instance is claimed**, because from
    /// then on `listener.management_listen_addr` is the answer.
    pub setup_listen: Option<std::net::IpAddr>,
}

/// One of W5 §3.6's recovery commands, run on the host rather than through the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `reset-password [--username <name>]` — the locked-out answer.
    ///
    /// Revokes **every** session, because the case it exists for is usually accompanied by
    /// not knowing what else is signed in.
    ResetPassword {
        /// The configuration database.
        config: PathBuf,
        /// Rename the administrator while resetting, or `None` to leave the name alone.
        username: Option<String>,
    },
    /// `reset-setup-token` — re-issue the one-time claim code for an instance that was never
    /// claimed and whose console output has scrolled away.
    ResetSetupToken {
        /// The configuration database.
        config: PathBuf,
    },
    /// `sessions revoke --all` — "I left myself signed in somewhere".
    RevokeSessions {
        /// The configuration database.
        config: PathBuf,
    },
}

/// A `migrate-config` command line.
///
/// Its own type rather than two more fields on [`Args`], because it is a different thing to
/// do: nothing binds, nothing is planned and the process exits when the migration is
/// written. [`MigrateArgs::args`] is the part the two share -- where the configuration
/// database is, and how to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateArgs {
    /// `--config <path>` (the destination), `--log-format` and `--log-level`.
    pub args: Args,
    /// `--from <path>`: the Go daemon's own configuration database. **Read only**, and
    /// never written to.
    pub from: PathBuf,
    /// `--dry-run`: apply everything inside a transaction, print the report,
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
    /// Run one of W5 §3.6's commands and exit.
    Command(Command),
    /// Translate a Go configuration database into the `--config` one, and exit.
    ///
    /// Its own variant rather than a [`Command`], even though it is spelled as one on the
    /// command line: W5 §3.6's commands run **before** any subscriber and print to the
    /// console, because what they have to say is one line to somebody who is locked out.
    /// The migration's report is dozens of lines about a deployment and belongs in the
    /// journal, so it installs the subscriber first and logs. Two plumbings, two variants.
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
    #[error("unrecognised argument `{arg}`")]
    Unknown {
        /// What was given.
        arg: String,
    },
    /// A flag that only applies to one command was used with another, or with the daemon.
    #[error("{flag} is only for `{command}`")]
    WrongCommand {
        /// The flag.
        flag: &'static str,
        /// Where it belongs.
        command: &'static str,
    },
    /// `{words}` is not a command this binary has.
    #[error(
        "`{words}` is not a command; expected reset-password, reset-setup-token, \
             `sessions revoke --all` or migrate-config"
    )]
    NoSuchCommand {
        /// What was typed.
        words: String,
    },
    /// `migrate-config` with no source to read.
    #[error(
        "`migrate-config` needs --from <path>: the RemoteWeather daemon's own \
         configuration database, which is what --config names for IT"
    )]
    MigrateNeedsSource,
    /// `sessions revoke` without `--all`.
    #[error("`sessions revoke` needs --all; there is no way to name one session from here")]
    NeedsAll,
    /// `--setup-listen` was given something that is not an address.
    #[error("`{value}` is not an address to listen on")]
    BadAddress {
        /// What was given.
        value: String,
    },
}

const USAGE: &str = "\
modernweather - weather station data collection and distribution

Usage: modernweather --config <path> [options]
       modernweather reset-password --config <path> [--username <name>]
       modernweather reset-setup-token --config <path>
       modernweather sessions revoke --all --config <path>

Options:
  --config <path>          The SQLite configuration database. Created if absent;
                           migrations are applied on every start. Required.
  --log-format <format>    pretty | json. Default: pretty on a terminal, JSON otherwise.
  --log-level <directive>  RUST_LOG syntax, e.g. `info,mw_station=debug`. Overrides
                           RUST_LOG. Adjustable at run time through the management API.
  --setup-listen <addr>    Bind the management API here while this daemon has no
                           administrator account, instead of 127.0.0.1. For a headless
                           install with no SSH access. Widens the first-run window
                           deliberately and logs a warning; ignored once an account exists.
  -h, --help               Print this and exit.
  -V, --version            Print the version and exit.

Commands, all run on this machine and all needing only read/write access to the
configuration database:
  reset-password           Set a new administrator password and sign out everywhere.
                           Prompts twice on a terminal; otherwise reads the new password
                           from standard input. --username also renames the account.
  reset-setup-token        Print a new one-time setup code for an instance that has no
                           administrator account yet.
  sessions revoke --all    Sign out of every browser.
  migrate-config --from <path>
                           Translate the RemoteWeather (Go) daemon's own SQLite
                           configuration database into the one --config names, then exit
                           without serving. The source is opened READ ONLY. --config must
                           not already hold configuration, and the spool is placed beside
                           it. Every value that was changed, that has no successor here, or
                           that you have to enter yourself is reported one line at a time.
                           Add --dry-run to do all of it inside a transaction and roll it
                           back, so the report is produced and nothing is kept.
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
    let mut setup_listen = None;
    let mut username = None;
    let mut all = false;
    let mut from: Option<PathBuf> = None;
    let mut dry_run = false;
    // The subcommand, as the words that were not flags. `sessions revoke` is two, which is
    // why this is a list rather than an `Option<String>`.
    let mut words: Vec<String> = Vec::new();
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
            "--from" => from = Some(PathBuf::from(value()?)),
            "--dry-run" => dry_run = true,
            "--setup-listen" => {
                let v = value()?.to_string_lossy().into_owned();
                // The bare address, not `addr:port`: the port is `listener.management_port`
                // and this flag widens the *bind*, it does not move the service. Accepting a
                // port here would be a second place the port is configured.
                let bare = v
                    .strip_prefix('[')
                    .and_then(|h| h.strip_suffix(']'))
                    .unwrap_or(&v);
                setup_listen = Some(
                    bare.parse()
                        .map_err(|_| ArgsError::BadAddress { value: v.clone() })?,
                );
            }
            "--username" => username = Some(value()?.to_string_lossy().into_owned()),
            "--all" => all = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(Parsed::Printed);
            }
            "-V" | "--version" => {
                println!("modernweather {}", env!("CARGO_PKG_VERSION"));
                return Ok(Parsed::Printed);
            }
            // A word rather than a flag: part of a subcommand. Collected rather than
            // matched here so that `--config` may appear on either side of it, which is what
            // every operator will type without thinking about it.
            word if !word.starts_with('-') => words.push(word.to_string()),
            _ => {
                return Err(ArgsError::Unknown {
                    arg: arg.to_string_lossy().into_owned(),
                })
            }
        }
    }
    let config = config.ok_or(ArgsError::NoConfig)?;
    let joined = words.join(" ");
    match joined.as_str() {
        "" => {
            // The daemon. The command-only flags are rejected rather than ignored: silently
            // accepting `--username` here would read as having renamed something.
            if from.is_some() {
                return Err(ArgsError::WrongCommand {
                    flag: "--from",
                    command: "migrate-config",
                });
            }
            // Without the word this would have STARTED THE DAEMON, which is the one outcome
            // somebody typing a dry-run flag did not want.
            if dry_run {
                return Err(ArgsError::WrongCommand {
                    flag: "--dry-run",
                    command: "migrate-config",
                });
            }
            if username.is_some() {
                return Err(ArgsError::WrongCommand {
                    flag: "--username",
                    command: "reset-password",
                });
            }
            if all {
                return Err(ArgsError::WrongCommand {
                    flag: "--all",
                    command: "sessions revoke",
                });
            }
            Ok(Parsed::Run(Args {
                config,
                format,
                directive,
                setup_listen,
            }))
        }
        "reset-password" => {
            reject_daemon_flags(setup_listen.is_some(), all)?;
            Ok(Parsed::Command(Command::ResetPassword { config, username }))
        }
        "reset-setup-token" => {
            reject_daemon_flags(setup_listen.is_some(), all)?;
            if username.is_some() {
                return Err(ArgsError::WrongCommand {
                    flag: "--username",
                    command: "reset-password",
                });
            }
            Ok(Parsed::Command(Command::ResetSetupToken { config }))
        }
        "sessions revoke" => {
            if setup_listen.is_some() {
                return Err(ArgsError::WrongCommand {
                    flag: "--setup-listen",
                    command: "modernweather --config <path>",
                });
            }
            if username.is_some() {
                return Err(ArgsError::WrongCommand {
                    flag: "--username",
                    command: "reset-password",
                });
            }
            if !all {
                return Err(ArgsError::NeedsAll);
            }
            Ok(Parsed::Command(Command::RevokeSessions { config }))
        }
        // M7-4. Spelled as a command for the three above's reason -- it runs instead of
        // the daemon -- and plumbed separately, because its report goes to the journal.
        "migrate-config" => {
            reject_daemon_flags(setup_listen.is_some(), all)?;
            if username.is_some() {
                return Err(ArgsError::WrongCommand {
                    flag: "--username",
                    command: "reset-password",
                });
            }
            let from = from.ok_or(ArgsError::MigrateNeedsSource)?;
            Ok(Parsed::MigrateConfig(MigrateArgs {
                args: Args {
                    config,
                    format,
                    directive,
                    setup_listen: None,
                },
                from,
                dry_run,
            }))
        }
        _ => Err(ArgsError::NoSuchCommand { words: joined }),
    }
}

/// Refuse the flags that belong to the daemon, or to another command, on a command.
fn reject_daemon_flags(setup_listen: bool, all: bool) -> Result<(), ArgsError> {
    if setup_listen {
        return Err(ArgsError::WrongCommand {
            flag: "--setup-listen",
            command: "modernweather --config <path>",
        });
    }
    if all {
        return Err(ArgsError::WrongCommand {
            flag: "--all",
            command: "sessions revoke",
        });
    }
    Ok(())
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

    fn parse_command(args: &[&str]) -> Command {
        match parse(args.iter().map(OsString::from)) {
            Ok(Parsed::Command(c)) => c,
            other => panic!("expected a command, got {other:?}"),
        }
    }

    /// W5 §3.6's three commands, with `--config` on either side of the words -- which is
    /// what an operator types without thinking about it.
    #[test]
    fn the_three_recovery_commands_parse_with_the_config_on_either_side() {
        assert_eq!(
            parse_command(&["reset-password", "--config", "/tmp/a.db"]),
            Command::ResetPassword {
                config: PathBuf::from("/tmp/a.db"),
                username: None
            }
        );
        assert_eq!(
            parse_command(&[
                "--config",
                "/tmp/a.db",
                "reset-password",
                "--username",
                "chris"
            ]),
            Command::ResetPassword {
                config: PathBuf::from("/tmp/a.db"),
                username: Some("chris".into())
            }
        );
        assert_eq!(
            parse_command(&["reset-setup-token", "--config=/tmp/a.db"]),
            Command::ResetSetupToken {
                config: PathBuf::from("/tmp/a.db")
            }
        );
        assert_eq!(
            parse_command(&["sessions", "revoke", "--all", "--config", "/tmp/a.db"]),
            Command::RevokeSessions {
                config: PathBuf::from("/tmp/a.db")
            }
        );
    }

    /// Every way of getting a command wrong, including the two that would otherwise be
    /// silently accepted and read as having done something.
    #[test]
    fn a_misspelled_command_or_a_misplaced_flag_is_refused_rather_than_ignored() {
        assert_eq!(
            parse_err(&["reset-passwd", "--config", "/tmp/a.db"]),
            ArgsError::NoSuchCommand {
                words: "reset-passwd".into()
            }
        );
        // `sessions revoke` with no `--all` does nothing, so it is an error rather than a
        // command that revoked nothing and said so.
        assert_eq!(
            parse_err(&["sessions", "revoke", "--config", "/tmp/a.db"]),
            ArgsError::NeedsAll
        );
        // A command-only flag on the daemon.
        assert_eq!(
            parse_err(&["--config", "/tmp/a.db", "--username", "chris"]),
            ArgsError::WrongCommand {
                flag: "--username",
                command: "reset-password"
            }
        );
        assert_eq!(
            parse_err(&["--config", "/tmp/a.db", "--all"]),
            ArgsError::WrongCommand {
                flag: "--all",
                command: "sessions revoke"
            }
        );
        // A daemon-only flag on a command.
        assert_eq!(
            parse_err(&[
                "reset-password",
                "--config",
                "/tmp/a.db",
                "--setup-listen",
                "0.0.0.0"
            ]),
            ArgsError::WrongCommand {
                flag: "--setup-listen",
                command: "modernweather --config <path>"
            }
        );
        // And a command still needs the database.
        assert_eq!(parse_err(&["reset-password"]), ArgsError::NoConfig);
    }

    /// `--setup-listen` takes a bare address, in both families, and refuses anything else.
    ///
    /// Bare rather than `addr:port`, because the port is `listener.management_port` and this
    /// flag widens the bind rather than moving the service -- accepting one here would be a
    /// second place the port is configured.
    #[test]
    fn setup_listen_takes_a_bare_address_in_either_family() {
        for (given, expected) in [
            ("0.0.0.0", "0.0.0.0"),
            ("192.168.1.10", "192.168.1.10"),
            ("::", "::"),
            ("[::1]", "::1"),
        ] {
            let args = parse_ok(&["--config", "/tmp/a.db", "--setup-listen", given]);
            assert_eq!(
                args.setup_listen.map(|a| a.to_string()).as_deref(),
                Some(expected),
                "{given}"
            );
        }
        for bad in ["weatherbox", "0.0.0.0:8081", ""] {
            assert_eq!(
                parse_err(&["--config", "/tmp/a.db", "--setup-listen", bad]),
                ArgsError::BadAddress {
                    value: bad.to_string()
                },
                "{bad}"
            );
        }
        // Absent is the normal case and is not a widened window.
        assert_eq!(parse_ok(&["--config", "/tmp/a.db"]).setup_listen, None);
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

    /// The command M7-4 adds, in both flag spellings, and the two combinations that are
    /// rejections rather than defaults.
    #[test]
    fn a_configuration_migration_is_its_own_command_line() {
        let m = match parse(
            [
                "migrate-config",
                "--config",
                "/var/lib/modernweather/config.db",
                "--from",
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

        // `--flag=value`, which a Dockerfile `CMD` array often writes, and the word AFTER
        // the flags -- which `words` collecting rather than matching is what buys.
        let m = match parse(
            [
                "--config=/d/config.db",
                "--from=/s/go.db",
                "--dry-run",
                "migrate-config",
            ]
            .iter()
            .map(OsString::from),
        ) {
            Ok(Parsed::MigrateConfig(m)) => m,
            other => panic!("expected a migration command line, got {other:?}"),
        };
        assert_eq!(m.from, PathBuf::from("/s/go.db"));
        assert!(m.dry_run);

        // The source is the whole input, so its absence is a rejection and not an empty
        // migration.
        let e = match parse(
            ["migrate-config", "--config", "/d/config.db"]
                .iter()
                .map(OsString::from),
        ) {
            Err(e) => e,
            other => panic!("expected a rejection, got {other:?}"),
        };
        assert_eq!(e, ArgsError::MigrateNeedsSource);

        // Without the word, these two flags would have STARTED THE DAEMON -- the one
        // outcome somebody typing them did not want.
        for flag in ["--dry-run", "--from=/s/go.db"] {
            let e = match parse(
                ["--config", "/d/config.db", flag]
                    .iter()
                    .map(OsString::from),
            ) {
                Err(e) => e,
                other => panic!("expected a rejection of {flag}, got {other:?}"),
            };
            assert!(
                matches!(e, ArgsError::WrongCommand { command, .. } if command == "migrate-config"),
                "{flag}: {e}"
            );
        }

        // And the ordinary command line is untouched.
        assert!(matches!(
            parse(["--config", "/d/config.db"].iter().map(OsString::from)),
            Ok(Parsed::Run(_))
        ));
    }

    /// The usage text names the command and its two flags, because an operator at cutover
    /// reads `--help` and not this file.
    #[test]
    fn the_usage_names_the_migration() {
        assert!(USAGE.contains("migrate-config --from <path>"));
        assert!(USAGE.contains("--dry-run"));
    }
}
