//! The two signals that mean "stop", and nothing else.
//!
//! W8's unit sets `KillSignal=SIGTERM` and `KillMode=mixed`, so SIGTERM is the one that
//! arrives in production; SIGINT is here because a developer running this in a terminal
//! presses Ctrl-C and should get the same ordered shutdown rather than a killed process
//! with a half-written spool.
//!
//! **The handler is installed before the boot sequence does any I/O, not when the main
//! loop starts.** Until a stream exists SIGTERM has its default disposition, so a
//! `systemctl stop` during startup killed the process outright -- no `STOPPING=1`, no
//! ordered shutdown, and "killed by signal TERM" in the journal. The window is every step
//! of the boot sequence: the migrations, the configuration read, the database probe and
//! the bind. One stream therefore covers startup and the run loop, which also means a
//! signal that arrives between them is not lost: `tokio`'s signal streams queue a pending
//! signal rather than dropping it.
//!
//! What happens next is §3.6: the supervisor walks the plan in **reverse topological
//! order** so a provider outlives its consumers, stops each rank's siblings concurrently,
//! and aborts anything that misses its grace. The default grace is 10 s, which is what
//! `TimeoutStopSec=30` in the unit leaves room for.

use std::future::Future;

use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// The shutdown signals, installed once and used for the whole life of the process.
#[derive(Debug)]
pub struct Signals {
    /// `None` when the handler could not be installed, which is reported and not fatal:
    /// the daemon still stops on Ctrl-C and on a cancelled token, and refusing to start
    /// because a signal handler is unavailable would be worse than the degradation.
    term: Option<Signal>,
}

impl Signals {
    /// Install the SIGTERM handler.
    pub fn install() -> Self {
        let term = match signal(SignalKind::terminate()) {
            Ok(stream) => Some(stream),
            Err(e) => {
                warn!(
                    err = %mw_types::DisplayChain(&e),
                    "SIGTERM cannot be handled; shutdown will rely on SIGINT"
                );
                None
            }
        };
        Signals { term }
    }

    /// Run a startup step, unless a shutdown signal arrives first.
    ///
    /// `None` means "stop instead": the token is cancelled and the caller should return
    /// without finishing the boot. A step that has already begun is dropped at its next
    /// await point, which is the same cancellation the supervisor applies to a component
    /// (§3.6) -- and every startup step here is idempotent, because the next start runs
    /// the same migrations and reads the same rows.
    pub async fn race<F: Future>(
        &mut self,
        shutdown: &CancellationToken,
        step: F,
    ) -> Option<F::Output> {
        let Some(term) = &mut self.term else {
            return Some(step.await);
        };
        tokio::select! {
            output = step => Some(output),
            _ = term.recv() => {
                info!(signal = "SIGTERM", "shutting down during startup");
                shutdown.cancel();
                None
            }
            result = tokio::signal::ctrl_c() => {
                if result.is_ok() {
                    info!(signal = "SIGINT", "shutting down during startup");
                }
                shutdown.cancel();
                None
            }
        }
    }

    /// Wait for a shutdown signal, then cancel the token.
    ///
    /// Also returns when the token is cancelled by something else -- a failed listener, or
    /// a future `ComponentControl` request -- so that the caller's `join!` cannot be left
    /// waiting on a signal nobody is going to send.
    pub async fn wait(mut self, shutdown: &CancellationToken) {
        let Some(term) = &mut self.term else {
            shutdown.cancelled().await;
            return;
        };
        let signal = tokio::select! {
            _ = term.recv() => "SIGTERM",
            result = tokio::signal::ctrl_c() => match result {
                Ok(()) => "SIGINT",
                Err(e) => {
                    warn!(err = %mw_types::DisplayChain(&e), "SIGINT cannot be handled");
                    shutdown.cancelled().await;
                    return;
                }
            },
            () = shutdown.cancelled() => return,
        };
        info!(signal, "shutting down");
        shutdown.cancel();
    }
}
