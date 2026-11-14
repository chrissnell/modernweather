//! W5 §3.6's three recovery commands: the answer to being locked out, without a backdoor.
//!
//! `reset-password`, `reset-setup-token` and `sessions revoke --all`, each run on the host
//! and each needing nothing but read/write access to the configuration database.
//!
//! # Why that is the right trust boundary, stated rather than assumed
//!
//! If you can run these, you can already read `config.db` — which holds the weather
//! database's URL and password, the Weather Underground password and the APRS passcode. A
//! recovery path **weaker** than the thing it protects would be a hole; one that is
//! **stronger** would be theatre. This is exactly as strong, and it is what makes "never
//! ship a default or backdoor credential" affordable: the locked-out case has an answer that
//! does not require one.
//!
//! # No subscriber, and no plan
//!
//! These run instead of the daemon, so there is no `tracing` subscriber, no supervisor and no
//! listener. Everything they say goes to the console through [`mw_obs::console_banner`] — the
//! same writer the first-run banner uses, and for the same reason: `ci/layers.toml`'s
//! `no-stdout-logging` rule allows the printing macros only on the binary's
//! argument-parsing path, and this is not it. (Its scan is a substring match over whole
//! files and does not know what a comment is, which is why those macros are not named here.)
//!
//! The absence of a plan is also why none of these three is an endpoint. A rename is
//! indistinguishable from a lockout from the outside — if the operator has forgotten which
//! the username was, the API cannot tell them — and re-issuing a setup code for a claimed
//! instance is refused by the store, so there would be nothing for an endpoint to do.

use std::io::IsTerminal;

use anyhow::Context as _;
use secrecy::{ExposeSecret, SecretString};
use sqlx::SqlitePool;

use crate::args::Command;

/// Run one command and say what happened.
pub async fn run(command: &Command) -> anyhow::Result<()> {
    match command {
        Command::ResetPassword { config, username } => {
            reset_password(config, username.as_deref()).await
        }
        Command::ResetSetupToken { config } => reset_setup_token(config).await,
        Command::RevokeSessions { config } => revoke_sessions(config).await,
    }
}

/// Open the configuration database, applying migrations.
///
/// Migrated rather than opened read-only, because a command run against a database from an
/// older build would otherwise fail on a column that is not there yet — and the migrations
/// are idempotent by ledger (**D-8**, A-W6-2), so this is the same thing the daemon does at
/// step 3 of its own boot.
async fn open(config: &std::path::Path) -> anyhow::Result<SqlitePool> {
    mw_store::config_db::open_and_migrate(config)
        .await
        .with_context(|| format!("opening the configuration database {}", config.display()))
}

/// `modernweather reset-password [--username <name>]`.
async fn reset_password(config: &std::path::Path, username: Option<&str>) -> anyhow::Result<()> {
    // Trimmed and refused if empty, for the reason `POST /api/v1/auth/claim` does the same:
    // `admin_credential.username` has no `CHECK` against an empty string, so a
    // `--username ""` would leave a credential that only an empty username can sign into --
    // which is a lockout produced by the command that exists to end one.
    let username = match username.map(str::trim) {
        Some("") => anyhow::bail!("--username needs a name; it cannot be empty"),
        other => other,
    };
    let pool = open(config).await?;
    let existing = mw_store::admin_auth::credential(&pool)
        .await
        .context("reading the administrator account")?;
    let Some(existing) = existing else {
        // Not an error with an exit code the operator has to interpret: this is the state
        // `reset-setup-token` is for, and saying so is more useful than "no such row".
        say(&[
            "This daemon has no administrator account yet, so there is no password to reset."
                .to_string(),
            "Run `modernweather reset-setup-token --config <path>` to print a setup code, \
             then create the account in the browser."
                .to_string(),
        ]);
        anyhow::bail!("there is no administrator account to reset");
    };

    let password = read_new_password()?;
    let problems = mw_api_mgmt::auth::password_problems(&password, "password");
    if !problems.is_empty() {
        for problem in &problems {
            say(std::slice::from_ref(&problem.message));
        }
        anyhow::bail!("the new password was refused");
    }
    // **O-10**'s answer is warn, not block — here as well as on the API, because a rule that
    // applied on one of the two surfaces would be a rule the other could be used to evade.
    for warning in mw_api_mgmt::auth::password_warnings(&password) {
        say(&[warning]);
    }

    let phc = mw_api_mgmt::auth::hash_password(&password)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("hashing the new password")?;
    let revoked = mw_store::admin_auth::reset_password(&pool, &phc, username)
        .await
        .context("writing the new administrator password")?;
    let name = username.unwrap_or(&existing.username);
    say(&[format!(
        "The password for `{name}` has been changed, and {revoked} session(s) were signed out."
    )]);
    pool.close().await;
    Ok(())
}

/// `modernweather reset-setup-token`.
async fn reset_setup_token(config: &std::path::Path) -> anyhow::Result<()> {
    let pool = open(config).await?;
    let issued = crate::authstore::reissue_setup_code(&pool)
        .await
        .context("issuing a setup code")?;
    if !issued {
        say(&[
            "This daemon already has an administrator account, so a setup code would do \
             nothing."
                .to_string(),
            "Run `modernweather reset-password --config <path>` to set a new password.".to_string(),
        ]);
        pool.close().await;
        anyhow::bail!("this instance has already been claimed");
    }
    pool.close().await;
    Ok(())
}

/// `modernweather sessions revoke --all`.
async fn revoke_sessions(config: &std::path::Path) -> anyhow::Result<()> {
    let pool = open(config).await?;
    let revoked = mw_store::admin_auth::revoke_all(&pool)
        .await
        .context("revoking every session")?;
    say(&[format!("{revoked} session(s) were signed out.")]);
    pool.close().await;
    Ok(())
}

/// Read a new password: twice from a terminal, once from a pipe.
///
/// §3.6: "Prompts twice on a TTY, writes a new Argon2id hash, and **revokes every session**.
/// Non-interactive use reads from stdin so it can be scripted."
///
/// The confirmation is only asked for interactively, and that is the decision rather than an
/// omission: a script that pipes the password in has already written it once, and asking for
/// it twice would mean a provisioning tool had to duplicate a secret in its own input.
fn read_new_password() -> anyhow::Result<SecretString> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let typed = read_line()?;
        // **The first line only**, which is what every password-reading tool does and what a
        // `printf 'secret\n' | ...` in a provisioning script means. Reading all of standard
        // input instead would quietly fold a trailing blank line into the password, and the
        // operator would find out at the next login. Only the terminator is trimmed: a
        // leading or trailing space is part of a password.
        anyhow::ensure!(
            !typed.expose_secret().is_empty(),
            "standard input was empty; a password is required"
        );
        return Ok(typed);
    }

    let echo = EchoOff::new();
    say(&[format!(
        "New password for the administrator account ({} characters or more, no other rules):",
        mw_api_mgmt::auth::POLICY_MIN_CHARS
    )]);
    let first = read_line()?;
    say(&["And again, to be sure:".to_string()]);
    let second = read_line()?;
    drop(echo);
    anyhow::ensure!(
        first.expose_secret() == second.expose_secret(),
        "those two passwords are not the same"
    );
    Ok(first)
}

/// One line from standard input, as a secret from the moment it exists (**D-33**).
fn read_line() -> anyhow::Result<SecretString> {
    let mut typed = String::new();
    std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut typed)
        .context("reading from the terminal")?;
    Ok(SecretString::from(typed.trim_end_matches(['\n', '\r'])))
}

/// Terminal echo, turned off for as long as this value lives.
///
/// **Through `stty` rather than through `termios`.** DESIGN.md D-32's table is the list of
/// crates this program may use and no terminal crate is in it — "a substitution is an
/// amendment, not a manifest edit" — and the alternative is an `ioctl`, which means `libc`
/// and `unsafe` in a workspace whose lints warn on both. `stty` is in `coreutils` and is on
/// every machine where a person is sitting at a console, which is the only situation this
/// code path runs in.
///
/// **If it is not there, the password is echoed and the operator is told so**, rather than
/// the command refusing. §3.6's whole purpose is that the locked-out case has an answer; a
/// recovery command that will not run on a minimal system does not have one. The exposure is
/// the terminal's scrollback on a machine the operator is physically at, and saying so is
/// what makes it their choice.
struct EchoOff {
    restore: bool,
}

impl EchoOff {
    fn new() -> Self {
        let off = stty("-echo");
        if !off {
            say(&["`stty` is not available here, so what you type will be visible.".to_string()]);
        }
        Self { restore: off }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        if self.restore {
            let _ = stty("echo");
            // The newline the terminal did not echo when the operator pressed return.
            say(&[String::new()]);
        }
    }
}

/// Ask `stty` to change this terminal's echo setting. `false` if it could not.
fn stty(flag: &str) -> bool {
    std::process::Command::new("stty")
        .arg(flag)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Say something to the operator.
///
/// Through the same writer as the first-run banner, for this module's documented reason.
fn say(lines: &[String]) {
    mw_obs::console_banner(lines);
}
