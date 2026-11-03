//! The error classification of docs/service-architecture.md §4.2, and the one renderer of
//! a cause chain from §4.3.
//!
//! **Why these two live in the lowest crate.** §4.2 requires a retry policy implemented
//! *once*, generically over `E: Classify`, and §4.1 puts error enums in nine crates
//! including `mw-codec` and `mw-aprs` — whose only permitted workspace dependency is this
//! one. A `Classify` defined anywhere higher could not be implemented beside
//! `CodecError`, which is where the knowledge of what `BadChecksum` means actually lives.
//! The same argument applies to [`DisplayChain`]: every crate logs, and §4.3's convention
//! is that exactly one thing knows how to walk a chain.
//!
//! The `tower` half — `ClassPolicy`, and the `Retry`-outside-`RateLimit` layer order — is
//! not here. It needs `tower`, and it belongs with the stacks that use it.

use std::error::Error;
use std::fmt;
use std::time::Duration;

/// What a caller should do about an error, decided **once, beside the error's
/// definition**, rather than re-decided at each call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Transient. `after` carries an externally supplied hint — a `Retry-After` header, a
    /// limiter's "not before T" — when the remote end said when.
    Retryable {
        /// How long to wait, when something told us.
        after: Option<Duration>,
    },
    /// A configuration or programming error. No retry can help; failing fast is the
    /// service.
    Fatal,
    /// An expected absence. Count it and continue; it is not an error at the caller's
    /// level. One corrupt serial frame is this; so is a rejected reading, which is a
    /// signal rather than a failure.
    Ignorable,
}

impl Class {
    /// Transient, with no hint about when.
    pub const RETRYABLE: Self = Class::Retryable { after: None };

    /// The label value for a metric or a log field.
    pub const fn as_str(self) -> &'static str {
        match self {
            Class::Retryable { .. } => "retryable",
            Class::Fatal => "fatal",
            Class::Ignorable => "ignorable",
        }
    }
}

/// An error that knows what should be done about it.
pub trait Classify {
    /// This error's classification.
    fn class(&self) -> Class;
}

/// Renders an error and every cause behind it, once.
///
/// §4.3's first convention is that `Display` on an error prints **one level only**, so
/// that the chain survives to be inspected rather than being flattened into a string at
/// each wrap. This is the one thing that walks it: `error!(err = %DisplayChain(&e), ...)`.
///
/// The measured defect this exists against: the Go implementation formats wrapped errors
/// with `%v` in 767 of its 1,174 error-construction sites, so the root cause of most of
/// its failures cannot be inspected programmatically at all.
pub struct DisplayChain<'a>(pub &'a dyn Error);

impl fmt::Debug for DisplayChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DisplayChain({self})")
    }
}

impl fmt::Display for DisplayChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(e) = source {
            write!(f, ": {e}")?;
            source = e.source();
        }
        Ok(())
    }
}

impl<'a> DisplayChain<'a> {
    /// The causes behind this error, outermost first, for the JSON layer's `causes` array.
    pub fn causes(&self) -> Vec<String> {
        let mut out = vec![self.0.to_string()];
        let mut source = self.0.source();
        while let Some(e) = source {
            out.push(e.to_string());
            source = e.source();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("inner")]
    struct Inner;

    #[derive(Debug, thiserror::Error)]
    #[error("outer")]
    struct Outer(#[source] Inner);

    impl Classify for Outer {
        fn class(&self) -> Class {
            Class::Fatal
        }
    }

    #[test]
    fn the_chain_is_walked_once_and_display_stays_one_level() {
        let e = Outer(Inner);
        assert_eq!(e.to_string(), "outer");
        assert_eq!(DisplayChain(&e).to_string(), "outer: inner");
        assert_eq!(DisplayChain(&e).causes(), vec!["outer", "inner"]);
        assert_eq!(e.class(), Class::Fatal);
    }
}
