//! The two signals that mean "stop", and nothing else.
//!
//! W8's unit sets `KillSignal=SIGTERM` and `KillMode=mixed`, so SIGTERM is the one that
//! arrives in production; SIGINT is here because a developer running this in a terminal
//! presses Ctrl-C and should get the same ordered shutdown rather than a killed process
//! with a half-written spool.
//!
//! What happens next is §3.6: the supervisor walks the plan in **reverse topological
//! order** so a provider outlives its consumers, stops each rank's siblings concurrently,
//! and aborts anything that misses its grace. The default grace is 10 s, which is what
//! `TimeoutStopSec=30` in the unit leaves room for.

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Wait for a shutdown signal, then cancel the token.
///
/// Also returns when the token is cancelled by something else -- a failed listener, or a
/// future `ComponentControl` request -- so that the caller's `join!` cannot be left
/// waiting on a signal nobody is going to send.
pub async fn wait(shutdown: &CancellationToken) {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(e) => {
            // Reported rather than fatal: the daemon still stops on Ctrl-C and on a
            // cancelled token, and refusing to start because a signal handler could not
            // be installed would be worse than the degradation.
            warn!(
                err = %mw_types::DisplayChain(&e),
                "SIGTERM cannot be handled; shutdown will rely on SIGINT"
            );
            shutdown.cancelled().await;
            return;
        }
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
