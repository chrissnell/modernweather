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
//! STATUS=...   "12 running, 2 waiting (db:postgres), 0 failed", when it changes
//! STOPPING=1   on shutdown
//! WATCHDOG=1   from this loop if WatchdogSec is set, at no less often than half the
//!              configured interval
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

/// Report which step of the boot sequence is running.
///
/// §3.5 lists `STATUS=` as a running commentary and the readiness loop sends it on every
/// transition -- but that loop only exists once the supervisor does, and the steps before
/// it are the slow ones: the migrations, and a database probe that waits on a network. A
/// `Type=notify` unit whose start is stuck otherwise shows an operator nothing at all
/// through `systemctl status`, which is the cheapest observability this protocol offers.
///
/// It is also the signal a test can wait for instead of guessing how long a start takes.
pub fn starting(step: &str) {
    send(&[NotifyState::Status(step)]);
}

/// Tell the service manager the process is stopping.
///
/// Public because the boot sequence needs it on the paths that never reach [`run`]: a
/// `systemctl stop` during startup is an ordered exit, and a `Type=notify` unit should
/// hear `STOPPING=1` for it rather than watching the process disappear. `READY=1` was
/// never sent on those paths, which is the honest thing to have not said.
pub fn stopping() {
    send(&[NotifyState::Stopping]);
}

/// Drive the notifications until shutdown.
///
/// Owns the periodic tick as well, because the two reasons to wake up -- "tell systemd we
/// are alive" and "do the small periodic jobs" -- are the same wake-up.
pub async fn run(
    mut readiness: watch::Receiver<Readiness>,
    obs: &Obs,
    shutdown: &CancellationToken,
) {
    // Half the configured interval, and never less often than [`TICK`] -- so the
    // cadence is `min(WatchdogSec / 2, 10s)` rather than exactly half. More often than
    // asked is always safe; less often is a kill.
    let watchdog = sd_notify::watchdog_enabled().map(|interval| interval / 2);
    if let Some(every) = watchdog {
        info!(
            every_s = every.as_secs(),
            "the service manager set a watchdog"
        );
    }
    let tick = watchdog.map_or(TICK, |w| w.min(TICK));

    let mut state = Announced::default();
    state.update(&readiness.borrow_and_update().clone());
    // A deadline rather than `sleep(tick)` inside the loop. `select!` drops the losing
    // branch, so a sleep constructed in the loop body is a *timeout* that every readiness
    // change resets -- and `publish_state` runs on every supervisor event, so a plan with
    // one component flapping in `Backoff` could starve this branch forever. What it drives
    // is §5.6's suppression summaries and the exporter's upkeep, which matter most exactly
    // when the log is busiest. `tokio::time::interval` would be the obvious answer and is
    // in `clippy.toml`'s disallowed list, because there is one clock for the whole process
    // (§3.3) and this is the binary's own loop rather than a second one.
    let mut next = tokio::time::Instant::now() + tick;
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
                let current = readiness.borrow_and_update().clone();
                state.update(&current);
            }
            () = tokio::time::sleep_until(next) => {
                next += tick;
                obs.tick();
                if watchdog.is_some() {
                    send(&[NotifyState::Watchdog]);
                }
            }
        }
    }
    stopping();
}

/// What has already been told to the service manager.
///
/// `READY=1` is latched because systemd takes the first one as the answer, and
/// `mw-supervisor` latches `converged` for the same reason: it is a fact about the past.
/// The status is remembered so that an unchanged one is not re-sent -- `publish_state`
/// runs on every supervisor event rather than only on a transition, and `send_replace`
/// marks the watch changed even when the value is identical, so without this the daemon
/// wrote a datagram per event. §3.5 asks for `STATUS=` "on every transition", and a
/// transition is what a changed status line is.
#[derive(Debug, Default)]
struct Announced {
    ready: bool,
    status: String,
}

impl Announced {
    /// Send what has changed, and nothing else.
    fn update(&mut self, state: &Readiness) {
        if state.converged && !self.ready {
            info!(status = %state.status, "converged; notifying readiness");
            send(&[NotifyState::Ready, NotifyState::Status(&state.status)]);
            self.ready = true;
            self.status = state.status.clone();
            return;
        }
        if !state.status.is_empty() && state.status != self.status {
            send(&[NotifyState::Status(&state.status)]);
            self.status = state.status.clone();
        }
    }
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
        let mut announced = Announced::default();
        announced.update(&readiness(false));
        assert!(
            !announced.ready,
            "a supervisor that has not converged is not ready"
        );
        announced.update(&readiness(true));
        assert!(announced.ready, "convergence announces");
        announced.update(&readiness(true));
        assert!(
            announced.ready,
            "and stays announced without sending READY=1 again"
        );
    }

    /// `publish_state` runs on every supervisor event, not only on a transition, so an
    /// unchanged status line must not become a datagram per event: `sd_notify::notify` is
    /// a blocking `sendto` on the task that also runs the supervisor.
    #[test]
    fn an_unchanged_status_is_not_resent() {
        let mut announced = Announced::default();
        announced.update(&readiness(true));
        let after_ready = announced.status.clone();
        announced.update(&readiness(true));
        assert_eq!(announced.status, after_ready, "nothing changed");

        let mut moved = Readiness {
            converged: true,
            status: "1 running, 0 waiting, 0 failed".into(),
        };
        announced.update(&moved);
        assert_eq!(announced.status, moved.status, "a real transition is sent");
        moved.status = "1 running, 1 waiting (db:postgres), 0 failed".into();
        announced.update(&moved);
        assert_eq!(announced.status, moved.status);
    }
}
