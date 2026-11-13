//! Reaching a database through whatever invocation can reach it.
//!
//! The source of mechanism (b)'s Go half is the production box, and the target is a pod in a
//! Kubernetes cluster. Neither is reachable by a bare `psql` from the same machine, so the
//! whole invocation is an input -- which is exactly what `PSQL_SOURCE` / `PSQL_TARGET` are in
//! `ci/db/migrate-from-go/migrate-from-go.sh`, for the same reason, and this is that override
//! with the same shape:
//!
//! ```text
//! MW_DIFF_GO_PSQL='kubectl exec -n modernweather db-0 -- env PGPASSWORD=... psql postgresql://...'
//! ```
//!
//! Split on whitespace and no quoting, which is the one thing to know before writing one.
//! A connection string with a space in it has to be passed through the environment of the
//! command instead -- and a password belongs there anyway.

use std::process::Command;

/// A `psql` invocation, as the argv the operator supplied.
#[derive(Debug, Clone)]
pub struct Psql {
    argv: Vec<String>,
    /// What this connection is, for the report and for an error message.
    pub label: String,
}

impl Psql {
    pub fn new(label: &str, command: &str) -> Result<Psql, String> {
        let argv: Vec<String> = command.split_whitespace().map(str::to_string).collect();
        if argv.is_empty() {
            return Err(format!("{label}: empty psql command"));
        }
        Ok(Psql {
            argv,
            label: label.to_string(),
        })
    }

    /// The command as it would be typed, for the report's provenance line.
    ///
    /// Anything that looks like a password assignment is replaced: the harness prints its own
    /// inputs so a reader can tell which two systems a verdict is about, and a report that is
    /// only publishable after somebody remembers to redact it will be published unredacted
    /// once.
    pub fn redacted(&self) -> String {
        self.argv
            .iter()
            .map(|a| match a.split_once('=') {
                Some((k, _)) if k.to_ascii_uppercase().contains("PASSWORD") => {
                    format!("{k}=<redacted>")
                }
                _ => a.clone(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Run one statement and return its rows, already split on tabs.
    ///
    /// `-P null=\N` is not optional: every column mechanism (b) compares is numeric, and
    /// without it a NULL and an empty field are the same bytes -- which would erase the one
    /// class of difference (b) exists to report (D-26).
    pub fn rows(&self, sql: &str) -> Result<Vec<Vec<String>>, String> {
        let mut cmd = Command::new(&self.argv[0]);
        cmd.args(&self.argv[1..]);
        cmd.args([
            "-X",
            "-q",
            "-t",
            "-A",
            "-F",
            "\t",
            "-P",
            r"null=\N",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
        ]);
        let out = cmd
            .output()
            .map_err(|e| format!("{}: could not run `{}`: {e}", self.label, self.argv[0]))?;
        if !out.status.success() {
            return Err(format!(
                "{}: psql exited {}: {}",
                self.label,
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.split('\t').map(str::to_string).collect())
            .collect())
    }
}

/// A station name, quoted for SQL.
///
/// Doubling the quote would be enough, and the allowlist is here anyway: these names come
/// from an operator's command line and reach a database that is read-only by decision rather
/// than by permission (W9 D-2). Every name in the fleet -- `CSI`, `Suncrest Air Quality`,
/// `snow`, and the slugs the new schema derives from them -- passes it.
pub fn quote_station(name: &str) -> Result<String, String> {
    if name.is_empty() {
        return Err("empty station name".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || " -_.".contains(c))
    {
        return Err(format!(
            "station `{name}`: only letters, digits, space, dash, underscore and dot are \
             accepted here"
        ));
    }
    Ok(format!("'{name}'"))
}
