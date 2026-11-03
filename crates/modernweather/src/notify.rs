//! Readiness, status and the watchdog, for `Type=notify` (§3.5).
//!
//! W8's unit is `Type=notify` and its assumption 3 says that if W3 does not implement
//! readiness the unit must revert to `Type=simple`. **It is implemented.** Mechanism: the
//! `sd-notify` crate, which writes a datagram to `$NOTIFY_SOCKET` and links nothing --
//! important, because every release target is a statically linked musl build and
//! `libsystemd` is not available in one. The same decision as §5.3's "no journald-native
//! layer", for the same reason.
//!
//! ```text
//! READY=1      once the first full reconciliation pass completes
//! STATUS=...   "12 running, 2 waiting (db:postgres), 0 failed", on every transition
//! STOPPING=1   on shutdown
//! WATCHDOG=1   from this loop if WatchdogSec is set, at half the configured interval
//! ```
//!
//! **`READY=1` is "the supervisor has converged", not "every component is `Running`".**
//! An uploader waiting on a dead database would otherwise block readiness forever and
//! systemd would kill a daemon that is working exactly as **D-27** specifies. `STATUS=`
//! carries the nuance, which is why it is sent on every transition rather than once.
//!
//! **`RELOADING=1` is not sent, because nothing can reload yet.** §3.5 pairs it with a
//! plan diff, and a plan diff arrives when something re-plans: a `ConfigAdmin` write bumps
//! the generation, the binary re-plans and pushes the new plan down `PlanSink`. The
//! management API that performs that write is implementation-plan.md M7-1, and the config
//! watch is M3-3's. Sending `RELOADING=1` from a path that cannot exist yet would be a
//! notification for an event that never happens. `mw-supervisor`'s own note asks for it to
//! arrive as another field on the readiness watch when it does, which is where the loop
//! that knows about the diff can put it.

use std::time::Duration;

use mw_obs::Obs;
use mw_supervisor::Readiness;
use sd_notify::NotifyState;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// How often the loop wakes when there is no watchdog to satisfy.
///
/// Ten seconds, because what this tick actually drives is §5.6's suppression summaries
/// (one a minute per key) and the exporter's upkeep. There is one clock for the whole
/// process (§3.3), so neither gets a timer of its own -- and `tokio::time::interval` is in
/// `clippy.toml`'s disallowed list for exactly that reason.
pub const TICK: Duration = Duration::from_secs(10);

/// Drive the notifications until shutdown.
///
/// Owns the periodic tick as well, because the two reasons to wake up -- "tell systemd we
/// are alive" and "do the small periodic jobs" -- are the same wake-up.
pub async fn run(
    mut readiness: watch::Receiver<Readiness>,
    obs: &Obs,
    shutdown: &CancellationToken,
) {
    // Half the configured interval, per §3.5: a watchdog satisfied at exactly the deadline
    // is a watchdog that fires on the first scheduling hiccup.
    let watchdog = sd_notify::watchdog_enabled().map(|interval| interval / 2);
    if let Some(every) = watchdog {
        info!(
            every_s = every.as_secs(),
            "the service manager set a watchdog"
        );
    }
    let tick = watchdog.map_or(TICK, |w| w.min(TICK));

    let mut announced = announce(&readiness.borrow_and_update().clone(), false);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            changed = readiness.changed() => {
                if changed.is_err() {
                    // The supervisor's loop has ended, so there are no more transitions
                    // to report. Waiting for the shutdown token rather than returning
                    // keeps `STOPPING=1` the last thing sent.
                    shutdown.cancelled().await;
                    break;
                }
                let state = readiness.borrow_and_update().clone();
                announced = announce(&state, announced);
            }
            () = tokio::time::sleep(tick) => {
                obs.tick();
                if watchdog.is_some() {
                    send(&[NotifyState::Watchdog]);
                }
            }
        }
    }
    send(&[NotifyState::Stopping]);
}

/// Send `READY=1` the first time the supervisor has converged, and `STATUS=` every time.
///
/// Returns whether readiness has now been announced. Latched, because systemd takes the
/// first `READY=1` as the answer and `mw-supervisor` latches `converged` for the same
/// reason: it is a fact about the past.
fn announce(state: &Readiness, already: bool) -> bool {
    if state.converged && !already {
        info!(status = %state.status, "converged; notifying readiness");
        send(&[NotifyState::Ready, NotifyState::Status(&state.status)]);
        return true;
    }
    if !state.status.is_empty() {
        send(&[NotifyState::Status(&state.status)]);
    }
    already
}

/// Notify, or do nothing when there is no service manager.
///
/// `sd_notify::notify` returns `Ok` with no socket set, so running outside systemd costs
/// one environment lookup. A *failed* write is logged at `WARN` and ignored: the daemon
/// works whether or not anything is listening, and the one case where it matters --
/// `Type=notify` with no `READY=1` -- is a start timeout systemd reports itself.
fn send(states: &[NotifyState]) {
    match sd_notify::notify(states) {
        Ok(()) => debug!(states = states.len(), "notified the service manager"),
        Err(e) => warn!(
            err = %mw_types::DisplayChain(&e),
            "the service manager could not be notified"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readiness(converged: bool) -> Readiness {
        Readiness {
            converged,
            status: "0 running, 0 waiting, 0 failed".into(),
        }
    }

    #[test]
    fn readiness_is_announced_once_and_only_after_convergence() {
        assert!(
            !announce(&readiness(false), false),
            "a supervisor that has not converged is not ready"
        );
        assert!(announce(&readiness(true), false), "convergence announces");
        assert!(
            announce(&readiness(true), true),
            "and stays announced without sending READY=1 again"
        );
    }
}
