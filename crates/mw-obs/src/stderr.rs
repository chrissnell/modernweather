//! The one place in the workspace that names the process's standard error.
//!
//! `ci/layers.toml`'s `no-stdout-logging` rule exempts exactly two paths: the binary's
//! argument-parsing module, which has to print `--help` before a subscriber exists, and
//! this file. Collecting the environment reads and the writer here is what makes that
//! exemption one file rather than one crate: §5.1's rule is "`tracing` macros only", and
//! the stream `tracing` writes to has to be named somewhere.

use std::io::IsTerminal;

use crate::format::{Requested, Shape};

/// `MW_LOG_FORMAT`, the environment half of the `--log-format` override (§5.3).
pub const FORMAT_VAR: &str = "MW_LOG_FORMAT";

/// Whether the process's standard error is a terminal.
pub fn is_terminal() -> bool {
    std::io::stderr().is_terminal()
}

/// Resolve the format from the environment and the terminal, with an explicit request
/// taking precedence.
///
/// `Err` carries the offending `MW_LOG_FORMAT` value: an unparseable one is reported
/// rather than ignored, because falling back to detection would make the typo invisible.
pub fn shape(requested: Option<Requested>) -> Result<Shape, String> {
    let from_env = match std::env::var(FORMAT_VAR) {
        Ok(v) if !v.is_empty() => Some(Requested::parse(&v).ok_or(v)?),
        _ => None,
    };
    Ok(Shape::resolve(
        requested.or(from_env),
        is_terminal(),
        // no-color.org: any non-empty value disables colour.
        std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
        // The conventional spelling of the opposite instruction, with "0" meaning the
        // operator turned it off rather than set it.
        std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0"),
    ))
}

/// Write one rendered record, followed by a newline, in one call.
///
/// One `write_all` under the lock is what keeps two threads from interleaving halves of a
/// line. A write failure is **ignored on purpose**: the destination is journald, a pipe or
/// a terminal, and the only thing a logging subsystem could do about a broken stderr is
/// try to log about it.
pub fn write_line(line: &str) {
    use std::io::Write;
    let out = std::io::stderr();
    let mut out = out.lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\n");
}
