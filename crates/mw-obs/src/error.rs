//! One error enum for this crate, classified beside its definition (§4.1, §4.2).
//!
//! Every variant here is `Fatal`, including the one that looks transient: a rejected
//! filter directive is the *operator's* mistake, made through the management API, and no
//! retry fixes it. What a `Fatal` from this crate means to a caller is "refuse the request
//! and leave logging exactly as it was", which is the property §5.5 asks for -- a typo
//! must not be able to leave the process with no logging at all.

use mw_types::{Class, Classify};

/// Why an observability operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ObsError {
    /// The directive is not valid `EnvFilter` syntax. Rejected **before** the reload lock
    /// is taken, so nothing changed.
    #[error("`{directive}` is not a valid filter directive: {reason}")]
    FilterParse {
        /// What was submitted.
        directive: String,
        /// What the parser said.
        reason: String,
    },

    /// The reload lock was poisoned by a panic while it was held.
    ///
    /// Named rather than unwrapped, because an `unwrap` here is the worst available
    /// failure for a logging subsystem: it turns one unrelated panic into a process with
    /// no way to change its log level, and then into a second panic in the management
    /// API. §5.5 requires the modify closure to be panic-free by construction, which is
    /// why this is expected to be unreachable and is still a variant.
    #[error("the filter reload handle is poisoned, so the log level cannot be changed")]
    ReloadPoisoned,

    /// The reload handle outlived its layer, which can only happen if the subscriber was
    /// never installed or has been dropped.
    #[error("the logging subscriber cannot be reconfigured")]
    ReloadUnavailable,

    /// `MW_LOG_FORMAT` or `--log-format` carried something that is not a format.
    ///
    /// Reported rather than ignored: an unparseable override that fell back to detection
    /// would make the typo invisible, and the point of the override is that the operator
    /// is telling us the detection got it wrong.
    #[error("`{value}` is not a log format; expected one of {expected}")]
    FormatUnknown {
        /// What was submitted.
        value: String,
        /// The accepted spellings.
        expected: &'static str,
    },

    /// A `tracing` subscriber is already installed in this process.
    #[error("a tracing subscriber is already installed in this process")]
    SubscriberInstalled {
        /// What `tracing` said.
        reason: String,
    },

    /// A `metrics` recorder is already installed in this process.
    ///
    /// Both of these are process-global and installable once; a second [`crate::install`]
    /// is a programming error rather than a condition.
    #[error("a metrics recorder is already installed in this process")]
    RecorderInstalled {
        /// What `metrics` said.
        reason: String,
    },
}

impl Classify for ObsError {
    /// All fatal. A bad directive is the operator's to fix and a double install is this
    /// build's to fix; neither becomes true by being retried.
    fn class(&self) -> Class {
        Class::Fatal
    }
}
