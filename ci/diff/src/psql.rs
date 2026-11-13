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
    /// The harness prints its own inputs so that a reader can tell which two systems a
    /// verdict is about, and a report that is only publishable after somebody remembers to
    /// redact it will be published unredacted once. So both places a password can be in a
    /// `psql` invocation are rewritten: a `PGPASSWORD=`-style assignment, and the userinfo of
    /// a connection URI -- which is the form `ci/conformance/go-target.sh` and
    /// `ci/db/migrate-from-go/lab/run-in-pod.sh` both build, so it is the likely one.
    pub fn redacted(&self) -> String {
        self.argv
            .iter()
            .map(|a| redact_arg(a))
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

/// One argv element with any password in it replaced.
fn redact_arg(arg: &str) -> String {
    // `scheme://userinfo@rest`: everything between `://` and the LAST `@` before the next
    // `/` is credentials. The last, because a password may itself contain an `@`.
    if let Some(scheme_end) = arg.find("://") {
        let (scheme, rest) = arg.split_at(scheme_end + 3);
        let authority_end = rest.find('/').unwrap_or(rest.len());
        if let Some(at) = rest[..authority_end].rfind('@') {
            let userinfo = &rest[..at];
            let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
            let had_password = userinfo.contains(':');
            return format!(
                "{scheme}{user}{}@{}",
                if had_password { ":<redacted>" } else { "" },
                &rest[at + 1..]
            );
        }
        return arg.to_string();
    }
    match arg.split_once('=') {
        Some((k, v)) if k.to_ascii_uppercase().contains("PASSWORD") => {
            // A `=` inside a value is still part of the value: only the first one splits.
            let _ = v;
            format!("{k}=<redacted>")
        }
        _ => arg.to_string(),
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

#[cfg(test)]
mod tests {
    use super::redact_arg;

    #[test]
    fn a_uri_password_does_not_reach_the_report() {
        assert_eq!(
            redact_arg("postgresql://weather:s3cr3t@box:5432/weather_v2_0_0"),
            "postgresql://weather:<redacted>@box:5432/weather_v2_0_0"
        );
        // An `@` inside the password still leaves the host intact.
        assert_eq!(
            redact_arg("postgresql://u:p@ss@box/db"),
            "postgresql://u:<redacted>@box/db"
        );
        // No password, no change -- and the user is kept, because which role read the data
        // is part of what the provenance line is for.
        assert_eq!(
            redact_arg("postgresql://weather@box/db"),
            "postgresql://weather@box/db"
        );
        assert_eq!(redact_arg("postgresql://box/db"), "postgresql://box/db");
    }

    #[test]
    fn an_assignment_password_does_not_reach_the_report() {
        assert_eq!(redact_arg("PGPASSWORD=s3cr3t"), "PGPASSWORD=<redacted>");
        assert_eq!(redact_arg("PGPASSWORD=a=b"), "PGPASSWORD=<redacted>");
        assert_eq!(redact_arg("PGHOST=box"), "PGHOST=box");
        assert_eq!(redact_arg("psql"), "psql");
    }
}
