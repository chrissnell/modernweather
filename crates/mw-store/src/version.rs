//! **D-62**: the database version is asserted at startup as well as in CI.
//!
//! D-42 decided that the required version is asserted in both places and only the CI half
//! had an owner -- `ci/sql/assert-timescaledb-version.sql`, run by the `database` job.
//! This is the other half, and it makes the same comparison, in the binary's boot
//! sequence, after the configuration pool opens and before `plan()` runs.
//!
//! Three of the storage design's load-bearing spike answers are version-gated, so a server
//! that silently predates them turns a design premise into a runtime surprise:
//!
//! - nullable `ADD COLUMN` on compressed chunks without decompression (TimescaleDB 2.11+,
//!   extended since) -- the entire "adding a metric is cheap" premise;
//! - a continuous aggregate built on another continuous aggregate (2.9+) -- the 1m/1h/1d
//!   hierarchy;
//! - `stats_agg` / `rollup`, which need `timescaledb_toolkit` and are absent from the
//!   plain `timescale/timescaledb` images.
//!
//! The third is a **warning rather than an error**, exactly as the SQL has it: W4 owns the
//! sum-and-count versus `stats_agg` decision (OQ-3) and both are correct, so this fails
//! only once a migration adds the extension -- at which point the migration is the
//! assertion.
//!
//! # Why the comparison is a pure function
//!
//! "Assert the version at startup" is only worth having if the failure message is right,
//! and the one thing a test cannot easily arrange is a PostgreSQL 15 to point at. So
//! [`probe`] gathers three facts over a connection and [`check`] decides; the decision and
//! every message it produces are testable against versions no container in this repository
//! runs.

use std::fmt;

use sqlx::{Connection, PgConnection, Row};

/// The required PostgreSQL major version.
pub const REQUIRED_PG_MAJOR: u32 = 17;

/// The required TimescaleDB version.
pub const REQUIRED_TIMESCALE: &str = "2.31.0";

/// A dotted version, compared the way PostgreSQL compares `int[]`.
///
/// `string_to_array(actual, '.')::int[] < string_to_array(required, '.')::int[]` is the
/// SQL's comparison and it is element-wise, then by length -- which is why `2.9.0` does
/// **not** sort above `2.31.0` the way a text comparison would. Reproducing that rule
/// rather than using a semver crate is deliberate: the two halves of D-42 have to agree,
/// and `2.31` versus `2.31.0` is exactly where a different rule would disagree.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(Vec<u32>);

impl Version {
    /// Parse a dotted version. `None` if any component is not a number.
    pub fn parse(s: &str) -> Option<Self> {
        s.split('.')
            .map(|p| p.parse::<u32>().ok())
            .collect::<Option<Vec<_>>>()
            .filter(|v| !v.is_empty())
            .map(Version)
    }

    /// The required TimescaleDB version, parsed.
    pub fn required_timescale() -> Self {
        Version::parse(REQUIRED_TIMESCALE).expect("REQUIRED_TIMESCALE is a literal")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, part) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{part}")?;
        }
        Ok(())
    }
}

/// What the server reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// `server_version_num / 10000`, which is the major version on PostgreSQL 10 and up.
    pub pg_major: u32,
    /// `extversion` from `pg_extension`, verbatim, or `None` when the extension is not
    /// installed -- which the SQL reports before anything else, because every other check
    /// is meaningless without it.
    ///
    /// The **raw string** rather than a parsed [`Version`], so that "present but
    /// unparseable" is representable and [`check`] can reach it in the same order the SQL
    /// does. Parsing in the probe instead made a `2.31.0-dev` extension on a PostgreSQL 15
    /// server report the version it could not read, where the SQL reports the PostgreSQL
    /// major first -- the same refusal, with the less useful half of the reason.
    pub timescale: Option<String>,
    /// Whether `timescaledb_toolkit` is installed. A warning, not a requirement.
    pub toolkit: bool,
}

impl fmt::Display for Observed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PostgreSQL {} / timescaledb ", self.pg_major)?;
        match &self.timescale {
            Some(v) => write!(f, "{v}")?,
            None => f.write_str("absent")?,
        }
        write!(f, " / toolkit {}", self.toolkit)
    }
}

impl Observed {
    /// What the server said, for a test or a diagnostic that wants to build one.
    pub fn new(pg_major: u32, timescale: Option<&str>, toolkit: bool) -> Self {
        Observed {
            pg_major,
            timescale: timescale.map(str::to_owned),
            toolkit,
        }
    }
}

/// Why the database is not one this build can use.
///
/// Every variant names the required version **and** the one found: a startup failure that
/// says only "unsupported database" leaves an operator with nothing to do but read the
/// source, which is the failure this assertion exists to replace.
#[derive(Debug, thiserror::Error)]
pub enum VersionError {
    #[error("the timescaledb extension is not installed; {required} or newer is required")]
    TimescaleAbsent {
        /// What this build needs.
        required: Version,
    },

    #[error("PostgreSQL {found} is older than the required {required}")]
    PostgresTooOld {
        /// What this build needs.
        required: u32,
        /// What the server reported.
        found: u32,
    },

    #[error("timescaledb {found} is older than the required {required}")]
    TimescaleTooOld {
        /// What this build needs.
        required: Version,
        /// What the server reported.
        found: Version,
    },

    #[error("the weather database at {redacted} could not be reached to check its version")]
    Unreachable {
        /// The URL with its password removed: this one is logged, and the configured URL
        /// carries a password (**D-33**).
        redacted: String,
        #[source]
        source: sqlx::Error,
    },

    #[error("the weather database did not answer the version query")]
    Query {
        #[source]
        source: sqlx::Error,
    },

    #[error("the server reported `{reported}`, which is not a version")]
    Unreadable {
        /// What `pg_extension.extversion` held.
        reported: String,
    },
}

impl mw_types::Classify for VersionError {
    /// A version that is wrong is fatal; a connection that is gone is not.
    ///
    /// A server that is too old does not become new enough by being asked again, and
    /// **D-62** wants that refused at startup rather than discovered at the first
    /// `ADD COLUMN`. A server that could not be reached is the ordinary case on a reboot,
    /// and refusing to boot for it is the failure **D-27** rejects -- which is the
    /// decision the binary reads off this impl rather than making a second time.
    ///
    /// `Query` is split rather than being wholly fatal, because it covers two unrelated
    /// things: the connection dying mid-query (a server restarting alongside us -- the
    /// same condition as `Unreachable`, reached one step later) and the query or its
    /// column types being wrong (a defect in this build, which no retry fixes).
    fn class(&self) -> mw_types::Class {
        match self {
            VersionError::Unreachable { .. } => mw_types::Class::RETRYABLE,
            VersionError::Query { source } if transport_failure(source) => {
                mw_types::Class::RETRYABLE
            }
            VersionError::TimescaleAbsent { .. }
            | VersionError::PostgresTooOld { .. }
            | VersionError::TimescaleTooOld { .. }
            | VersionError::Query { .. }
            | VersionError::Unreadable { .. } => mw_types::Class::Fatal,
        }
    }
}

/// Whether a `sqlx` failure is the connection rather than the statement.
///
/// The distinction is the whole of `Query`'s classification: everything here is something
/// that happened to the socket or the session, and everything else -- the server rejecting
/// the statement, a column that is not there, a value that will not decode -- is this
/// build disagreeing with the server it was built against.
fn transport_failure(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::Protocol(_)
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            | sqlx::Error::WorkerCrashed
    )
}

/// The comparison `ci/sql/assert-timescaledb-version.sql` makes, in the order it makes it.
///
/// `Ok(false)` means "usable, but `timescaledb_toolkit` is absent" -- the SQL's `RAISE
/// WARNING`, which the caller logs.
pub fn check(observed: &Observed) -> Result<bool, VersionError> {
    let Some(reported) = observed.timescale.as_deref() else {
        return Err(VersionError::TimescaleAbsent {
            required: Version::required_timescale(),
        });
    };
    if observed.pg_major < REQUIRED_PG_MAJOR {
        return Err(VersionError::PostgresTooOld {
            required: REQUIRED_PG_MAJOR,
            found: observed.pg_major,
        });
    }
    // After the PostgreSQL comparison, because that is where the SQL reaches
    // `string_to_array` -- which raises `invalid input syntax for type integer` on an
    // unparseable version just as this returns `Unreadable`.
    let found = Version::parse(reported).ok_or_else(|| VersionError::Unreadable {
        reported: reported.to_owned(),
    })?;
    let required = Version::required_timescale();
    if found < required {
        return Err(VersionError::TimescaleTooOld { required, found });
    }
    Ok(observed.toolkit)
}

/// Ask the server the three questions.
///
/// One connection rather than a pool: this runs once, before `plan()`, and the pool that
/// serves the read path belongs to the `db:postgres` component (W2a Decision 34), which
/// does not exist yet. Opening a second pool here would be a second owner of the same
/// resource.
pub async fn probe(url: &str) -> Result<Observed, VersionError> {
    let mut conn =
        PgConnection::connect(url)
            .await
            .map_err(|source| VersionError::Unreachable {
                redacted: redact(url),
                source,
            })?;
    let answered = ask(&mut conn).await;
    // Closed rather than dropped on **both** paths, so the server does not log an
    // unexpected EOF -- including when the query is what failed, which is the path that
    // used to produce exactly the EOF this exists to avoid.
    let _ = conn.close().await;
    answered
}

/// The three questions, over an open connection.
async fn ask(conn: &mut PgConnection) -> Result<Observed, VersionError> {
    let row = sqlx::query(
        "SELECT current_setting('server_version_num')::int / 10000 AS pg_major, \
                (SELECT extversion FROM pg_extension WHERE extname = 'timescaledb') AS ts, \
                EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'timescaledb_toolkit') \
                    AS toolkit",
    )
    .fetch_one(conn)
    .await
    .map_err(|source| VersionError::Query { source })?;
    let pg_major: i32 = row
        .try_get("pg_major")
        .map_err(|source| VersionError::Query { source })?;
    let timescale: Option<String> = row
        .try_get("ts")
        .map_err(|source| VersionError::Query { source })?;
    let toolkit: bool = row
        .try_get("toolkit")
        .map_err(|source| VersionError::Query { source })?;
    Ok(Observed {
        // `server_version_num / 10000` is never negative, so this cannot be reached --
        // and a zero-fill would make the message read "PostgreSQL 0 is older than the
        // required 17", which is the kind of number this module exists to not print.
        pg_major: u32::try_from(pg_major).map_err(|_| VersionError::Unreadable {
            reported: pg_major.to_string(),
        })?,
        timescale,
        toolkit,
    })
}

/// A connection URL with every credential it can carry removed.
///
/// The configured URL is a secret from the moment it is read (**D-33**), and this is the
/// only form of it that may reach a log line -- which the `Unreachable` message does, on
/// the most ordinary failure there is. So this has to be right for every URL shape the
/// driver accepts, not just the obvious one:
///
/// - **userinfo**: `postgres://mw:hunter2@host/db`. The authority ends at the first `/`,
///   `?` or `#` (RFC 3986) and userinfo ends at the **last** `@` inside it, so an `@` in
///   the path or the query is not mistaken for a credential separator -- which also keeps
///   `?application_name=mw@node1` readable instead of garbled.
/// - **the query string**: `postgres://host/db?password=hunter2`. `sqlx` reads `password`
///   from there (`sqlx-postgres`'s own URL parser does, and so does libpq), and the
///   schema's `CHECK (url GLOB 'postgres://?*')` permits it -- so a URL written that way
///   was previously returned *verbatim*, password and all, because there was no `@` to
///   split on.
///
/// Textual rather than parsed, still: a URL crate for one error message is the trade the
/// original comment refused, and the two rules above are the whole of it.
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<not a url>".to_string();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let authority = match authority.rsplit_once('@') {
        Some((userinfo, host)) => {
            let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
            format!("{user}:{REDACTED}@{host}")
        }
        None => authority.to_string(),
    };
    format!("{scheme}://{authority}{}", redact_query(tail))
}

/// What replaces a secret.
const REDACTED: &str = "***";

/// Every parameter name that carries a credential in a PostgreSQL URL.
///
/// `sslpassword` is here as well as `password` because it is the passphrase for the client
/// key, which is no less a secret for being about TLS.
const SECRET_PARAMS: [&str; 2] = ["password", "sslpassword"];

/// Replace the value of any secret-bearing query parameter.
fn redact_query(tail: &str) -> String {
    let Some((path, query)) = tail.split_once('?') else {
        return tail.to_string();
    };
    // The fragment is not part of the query, and a `#` inside one would otherwise make the
    // last parameter look longer than it is.
    let (query, fragment) = match query.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (query, None),
    };
    let redacted = query
        .split('&')
        .map(|pair| {
            let name = pair.split_once('=').map_or(pair, |(n, _)| n);
            if SECRET_PARAMS.contains(&name) {
                format!("{name}={REDACTED}")
            } else {
                pair.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    match fragment {
        Some(f) => format!("{path}?{redacted}#{f}"),
        None => format!("{path}?{redacted}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observed(pg: u32, ts: Option<&str>, toolkit: bool) -> Observed {
        Observed::new(pg, ts, toolkit)
    }

    #[test]
    fn the_shipped_pair_is_accepted_and_the_toolkit_is_only_a_warning() {
        assert!(
            check(&observed(17, Some("2.31.0"), true)).expect("the shipped pair is accepted"),
            "the toolkit is reported as present"
        );
        assert!(
            !check(&observed(17, Some("2.31.0"), false)).expect("usable without the toolkit"),
            "W4 owns the stats_agg decision, so an absent toolkit is a warning"
        );
    }

    #[test]
    fn a_wrong_postgres_version_names_both_versions() {
        let e = check(&observed(15, Some("2.31.0"), true)).expect_err("too old");
        let message = e.to_string();
        assert!(
            message.contains("15"),
            "the found version is named: {message}"
        );
        assert!(
            message.contains("17"),
            "the required version is named: {message}"
        );
    }

    #[test]
    fn a_wrong_timescale_version_names_both_versions() {
        let e = check(&observed(17, Some("2.17.2"), true)).expect_err("too old");
        let message = e.to_string();
        assert!(message.contains("2.17.2"), "{message}");
        assert!(message.contains("2.31.0"), "{message}");
    }

    #[test]
    fn an_absent_extension_is_reported_before_anything_else() {
        // The SQL checks this first on purpose: with no extension installed, "PostgreSQL
        // 15 is older than 17" is true but useless.
        let e = check(&observed(15, None, false)).expect_err("no extension");
        assert!(
            matches!(e, VersionError::TimescaleAbsent { .. }),
            "{e:?} should be the absent-extension failure"
        );
        assert!(e.to_string().contains("2.31.0"), "{e}");
    }

    /// The SQL reaches `string_to_array` only after the PostgreSQL comparison, so a
    /// server that is behind on both reports the version an operator can act on.
    #[test]
    fn an_unreadable_extension_version_is_reported_after_the_postgres_comparison() {
        let e = check(&observed(15, Some("2.31.0-dev"), true)).expect_err("too old");
        assert!(
            matches!(e, VersionError::PostgresTooOld { .. }),
            "{e:?}: the SQL compares the major first"
        );
        let e = check(&observed(17, Some("2.31.0-dev"), true)).expect_err("unreadable");
        assert!(
            matches!(e, VersionError::Unreadable { .. }),
            "{e:?}: and reports what it could not read when that is all that is left"
        );
        assert!(e.to_string().contains("2.31.0-dev"), "{e}");
    }

    #[test]
    fn versions_compare_element_wise_and_not_as_text() {
        let v = |s| Version::parse(s).expect("a literal");
        assert!(v("2.9.0") < v("2.31.0"), "the defect a text comparison has");
        assert!(
            v("2.31") < v("2.31.0"),
            "shorter sorts first, as int[] does"
        );
        assert!(v("3.0.0") > v("2.31.0"));
        assert_eq!(v("2.31.0").to_string(), "2.31.0");
        assert_eq!(Version::parse("2.x"), None);
        assert_eq!(Version::parse(""), None);
    }

    /// The two halves of **D-42** have to agree, and nothing but this test makes them.
    ///
    /// `check` reproduces `ci/sql/assert-timescaledb-version.sql`'s comparison, so the
    /// required versions live in two files. Rather than leave that to be noticed during an
    /// upgrade, the constants are read out of the SQL and compared.
    #[test]
    fn the_required_versions_match_the_sql_that_ci_runs() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../ci/sql/assert-timescaledb-version.sql");
        let sql = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let value = |decl: &str| {
            sql.lines()
                .find_map(|l| l.trim().strip_prefix(decl))
                .map(|rest| {
                    rest.trim_start_matches([' ', ':', '='])
                        .trim()
                        .trim_end_matches(';')
                        .trim_matches('\'')
                        .to_string()
                })
                .unwrap_or_else(|| panic!("{decl} is not declared in {}", path.display()))
        };
        assert_eq!(
            value("required_pg      int"),
            REQUIRED_PG_MAJOR.to_string(),
            "the SQL and this module disagree about the PostgreSQL version"
        );
        assert_eq!(
            value("required_ts      text"),
            REQUIRED_TIMESCALE,
            "the SQL and this module disagree about the TimescaleDB version"
        );
    }

    /// The `Query` split is what keeps the D-62 x D-27 reading honest: a server that
    /// restarts between the connect and the query must not refuse the boot, and a query
    /// this build got wrong must.
    #[test]
    fn a_connection_lost_mid_query_is_retryable_and_a_bad_query_is_not() {
        use mw_types::{Class, Classify};
        let lost = VersionError::Query {
            source: sqlx::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
        };
        assert!(
            matches!(lost.class(), Class::Retryable { .. }),
            "a reset connection is the same condition as Unreachable, one step later"
        );
        let wrong = VersionError::Query {
            source: sqlx::Error::ColumnNotFound("pg_major".into()),
        };
        assert_eq!(
            wrong.class(),
            Class::Fatal,
            "a column this build asked for and did not get is a defect, not a condition"
        );
    }

    /// The test's name is a promise, so it covers every shape that can carry one rather
    /// than the one shape that is easy to write.
    #[test]
    fn a_password_never_reaches_a_log_line() {
        for (url, expected) in [
            // Userinfo, the obvious shape.
            (
                "postgres://mw:hunter2@db.internal:5432/weather",
                "postgres://mw:***@db.internal:5432/weather",
            ),
            // The query string, which `sqlx` and libpq both read and which used to come
            // back verbatim because there was no `@` to split on.
            (
                "postgres://db.internal/weather?password=hunter2",
                "postgres://db.internal/weather?password=***",
            ),
            (
                "postgres://db.internal/weather?user=mw&password=hunter2&sslmode=require",
                "postgres://db.internal/weather?user=mw&password=***&sslmode=require",
            ),
            // The TLS key's passphrase is a secret too.
            (
                "postgres://db.internal/weather?sslpassword=hunter2",
                "postgres://db.internal/weather?sslpassword=***",
            ),
            // Both at once.
            (
                "postgresql://mw:a@db/weather?password=b",
                "postgresql://mw:***@db/weather?password=***",
            ),
            // No credential at all, unchanged.
            (
                "postgresql://db.internal/weather",
                "postgresql://db.internal/weather",
            ),
            // An `@` after the authority is not a credential separator: the message
            // exists to say WHICH database could not be reached, so garbling the host is
            // its own defect.
            (
                "postgres://db.internal/weather?application_name=mw@node1",
                "postgres://db.internal/weather?application_name=mw@node1",
            ),
            // A user with no password, and a `@` inside the userinfo.
            ("postgres://mw@db/weather", "postgres://mw:***@db/weather"),
            (
                "postgres://mw%40corp:p@db/weather",
                "postgres://mw%40corp:***@db/weather",
            ),
        ] {
            assert_eq!(redact(url), expected, "redacting {url}");
            assert!(
                !redact(url).contains("hunter2"),
                "the password survived in {}",
                redact(url)
            );
        }
        assert_eq!(redact("nonsense"), "<not a url>");
    }
}
