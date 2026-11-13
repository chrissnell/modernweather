//! Where each side's bytes come from, and why that is an input rather than a constant.
//!
//! W1 **D-10**, which M5-2 inherits as its test strategy: a differ that cannot be made to
//! differ proves nothing. The only way to make this one differ on demand is to feed it inputs
//! somebody controls, so every side of every mechanism is one of two things -- a live system,
//! or a directory of recorded responses. The differ cannot tell which, and
//! `ci/diff/self-test.sh` is the negative control that runs it end to end over committed
//! inputs with no database, no network and no production.
//!
//! A recorded response is an envelope rather than a raw body:
//!
//! ```json
//! { "status": 200, "json": [ { "stationname": "CSI", "ts": 1794584700000 } ] }
//! ```
//!
//! because the status is part of what (c) compares -- `/snow` 404s for a website with snow
//! disabled and `/span` is a 500 for a station whose `winddir` is NULL throughout the window
//! (W1-BUG-1), and a recorded input that could not hold those could not reproduce them. A
//! body that is not JSON -- which is every one of the Go server's four error conventions --
//! is recorded as `"text"` instead of `"json"`, so a 404 corpus is the bytes the server sent
//! rather than an approximation of them.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// One side of mechanism (c).
#[derive(Debug, Clone)]
pub enum HttpSide {
    /// A base URL: `https://suncrestweather.com`, or a cluster address for the new daemon.
    Live { base: String },
    /// A directory of recorded envelopes, named by the slug the caller asks for.
    Recorded { dir: PathBuf },
}

/// One response, from either kind of side.
pub struct Fetched {
    pub status: u16,
    /// `None` when the body was not JSON, which is itself a comparable fact: the Go server
    /// serves `text/plain` error bodies on four different conventions.
    pub json: Option<serde_json::Value>,
    pub text: String,
}

impl HttpSide {
    pub fn parse(spec: &str) -> Result<HttpSide, String> {
        match spec.split_once(':') {
            Some(("dir", p)) => Ok(HttpSide::Recorded {
                dir: PathBuf::from(p),
            }),
            Some(("url", u)) => Ok(HttpSide::Live {
                base: u.trim_end_matches('/').to_string(),
            }),
            // A bare `https://...` is the common case and spelling it `url:` as well would be
            // noise; anything else has to say which it is.
            _ if spec.starts_with("http://") || spec.starts_with("https://") => {
                Ok(HttpSide::Live {
                    base: spec.trim_end_matches('/').to_string(),
                })
            }
            _ => Err(format!(
                "{spec}: expected a base URL, `url:<base>` or `dir:<path>`"
            )),
        }
    }

    /// What this side is, for the report's provenance line.
    pub fn describe(&self) -> String {
        match self {
            HttpSide::Live { base } => base.clone(),
            HttpSide::Recorded { dir } => format!("dir:{}", dir.display()),
        }
    }

    /// Fetch one endpoint. `slug` names the recorded file; `path` is the live request.
    ///
    /// Both are given by the caller rather than derived from each other, so that a recorded
    /// corpus is reviewable by filename and a live request is exactly the URL a reader can
    /// paste into `curl`.
    pub fn fetch(&self, slug: &str, path: &str) -> Result<Fetched, String> {
        match self {
            HttpSide::Recorded { dir } => read_recorded(&dir.join(format!("{slug}.json"))),
            HttpSide::Live { base } => {
                let url = format!("{base}{path}");
                // A long timeout on purpose: `/span/720h` against a year of daily buckets is
                // a real query on both sides, and a harness that timed out would report a
                // transport failure where the answer was "this is slow".
                let agent = ureq::AgentBuilder::new()
                    .timeout_read(Duration::from_secs(120))
                    .timeout_connect(Duration::from_secs(20))
                    .build();
                let resp = match agent.get(&url).call() {
                    Ok(r) => r,
                    // A 4xx/5xx is a response and not an error here: both are things (c)
                    // compares, and treating them as failures would hide a status difference
                    // behind a transport message.
                    Err(ureq::Error::Status(_, r)) => r,
                    Err(e) => return Err(format!("{url}: {e}")),
                };
                let status = resp.status();
                let text = resp.into_string().map_err(|e| format!("{url}: {e}"))?;
                Ok(Fetched {
                    status,
                    json: serde_json::from_str(&text).ok(),
                    text,
                })
            }
        }
    }
}

fn read_recorded(path: &Path) -> Result<Fetched, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let env: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: not JSON: {e}", path.display()))?;
    let status = env
        .get("status")
        .and_then(|s| s.as_u64())
        .ok_or_else(|| format!("{}: no numeric `status`", path.display()))?;
    let json = env.get("json").cloned();
    let text = match (&json, env.get("text").and_then(|t| t.as_str())) {
        (Some(j), _) => j.to_string(),
        (None, Some(t)) => t.to_string(),
        (None, None) => {
            return Err(format!(
                "{}: an envelope needs `json` or `text`. A recorded response with neither is \
                 not a response, and a harness that read it as an empty body would compare \
                 two absences and call them equal",
                path.display()
            ))
        }
    };
    Ok(Fetched {
        status: status as u16,
        json,
        text,
    })
}

/// One side of mechanism (b).
#[derive(Debug, Clone)]
pub enum RowSide {
    /// A `psql` invocation, which may be a `kubectl exec` into a pod.
    Psql { command: String },
    /// A recorded dump: a header line of column names, then one tab-separated row per
    /// reading, with `\N` for NULL. Exactly what `psql -tA -F'\t' -P null=\N` writes, so a
    /// corpus can be recorded with the tool the harness already runs.
    Tsv { path: PathBuf },
}

impl RowSide {
    pub fn parse(spec: &str) -> Result<RowSide, String> {
        match spec.split_once(':') {
            Some(("tsv", p)) => Ok(RowSide::Tsv {
                path: PathBuf::from(p),
            }),
            Some(("psql", c)) => Ok(RowSide::Psql {
                command: c.to_string(),
            }),
            _ => Err(format!("{spec}: expected `psql:<command>` or `tsv:<path>`")),
        }
    }
}
