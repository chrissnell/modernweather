//! `LogControl` over `mw-obs`'s reload handle (**D-52**).
//!
//! The implementation is here and not in `mw-obs` for a boundary reason rather than a
//! taste one: `ci/layers.toml` puts `mw-obs` in the `pure` layer and `mw-component` --
//! where the trait is declared -- in `component`, so `mw-obs` cannot name the trait and
//! `mw-component` cannot name `tracing_subscriber`. The binary is the one place a
//! capability consumer and its provider meet (§1.3 rule 1), and this is one of them.
//!
//! It is reached through `ComponentControl` rather than being a capability of its own:
//! one consumer, same root provider, and a thirteenth capability for one consumer is noise
//! in a graph whose only jobs are ordering and `Waiting` (**D-52**).

use std::sync::Arc;

use mw_component::{LogControl, LogControlError};
use mw_obs::{ObsError, Reload};
use tracing::warn;

/// The supervisor's view of the logging subsystem.
#[derive(Debug)]
pub struct ObsLogControl(Arc<Reload>);

impl ObsLogControl {
    /// Wrap the handle `mw_obs::install` returned.
    pub fn new(reload: Arc<Reload>) -> Self {
        ObsLogControl(reload)
    }
}

impl LogControl for ObsLogControl {
    fn current(&self) -> String {
        self.0.current()
    }

    fn set(&self, directive: &str) -> Result<(), LogControlError> {
        self.0.set(directive).map_err(translate)
    }

    fn reset(&self) -> Result<(), LogControlError> {
        self.0.reset().map_err(translate)
    }
}

/// `ObsError` carries cases the capability's caller cannot act on differently.
///
/// A poisoned lock and a dropped subscriber are both "this process cannot be
/// reconfigured", which is exactly `Unavailable`; the management API's 500 says the same
/// thing for either. What must survive the translation is the *parse* failure, because
/// that one is the operator's typo and the message is the useful half of the response.
///
/// `LogControlError::Unavailable` carries no source, so the collapsed cases are **logged
/// here** rather than disappearing: a poisoned reload lock would otherwise become a 500
/// reading "the logging subscriber cannot be reconfigured" with no trace of why anywhere
/// in the process. The one place that still knows is this function.
fn translate(error: ObsError) -> LogControlError {
    match error {
        ObsError::FilterParse { directive, reason } => {
            LogControlError::FilterParse { directive, reason }
        }
        collapsed => {
            warn!(
                err = %mw_types::DisplayChain(&collapsed),
                "the log level could not be changed"
            );
            LogControlError::Unavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one case whose message is the useful half of the management API's response
    /// survives; everything else collapses, and the collapse is what gets logged.
    #[test]
    fn a_rejected_directive_keeps_its_reason_and_the_rest_collapse() {
        let parse = translate(ObsError::FilterParse {
            directive: "mw_station=nonsense".into(),
            reason: "invalid filter directive".into(),
        });
        match parse {
            LogControlError::FilterParse { directive, reason } => {
                assert_eq!(directive, "mw_station=nonsense");
                assert_eq!(reason, "invalid filter directive");
            }
            other => panic!("the operator's typo must survive, got {other:?}"),
        }

        for collapsed in [
            ObsError::ReloadPoisoned,
            ObsError::ReloadUnavailable,
            ObsError::SubscriberInstalled {
                reason: "already".into(),
            },
            ObsError::RecorderInstalled {
                reason: "already".into(),
            },
            ObsError::FormatUnknown {
                value: "verbose".into(),
                expected: mw_obs::Requested::SPELLINGS,
            },
        ] {
            assert!(
                matches!(translate(collapsed), LogControlError::Unavailable),
                "everything that is not the operator's mistake is one answer"
            );
        }
    }

    // The other direction -- this capability over the *real* reload handle -- is
    // `crates/mw-obs/tests/install.rs`, which drives `Obs::log` through the installed
    // stack. It cannot be tested from here: constructing a `Reload` needs
    // `tracing_subscriber`, and only `mw-obs` may name it (`ci/layers.toml`).
}
