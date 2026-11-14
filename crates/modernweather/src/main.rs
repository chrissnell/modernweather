//! The ModernWeather daemon.
//!
//! The only crate that may depend on all the others, and the only one that may depend on
//! `anyhow` (docs/service-architecture.md §1.2, §4). It is where a capability consumer and
//! its provider meet: the binary resolves handles, calls `plan()` and runs the supervisor,
//! which is what lets every other crate name a trait instead of an implementation (§1.3).
//!
//! The boot sequence is [`boot`], in the order W3 §3.1 writes it. This file is only the
//! entry point and the exit code, because there are exactly three outcomes and they are
//! worth keeping visible:
//!
//! - **2** -- the command line was wrong. Nothing was opened and nothing was logged,
//!   because there was nothing to log with yet.
//! - **1** -- the daemon could not start, or could not stay started. The reason is on
//!   stderr either way: through the subscriber if there was one, and through
//!   [`args::fatal`] if installing the subscriber is what failed.
//! - **0** -- it ran and stopped cleanly, or one of W5 §3.6's recovery commands did its job.
//!
//! `Restart=always` in W8's unit means systemd restarts on all three, which is correct for
//! 1 and would be an infinite loop for 2 -- except that 2 can only come from the unit's
//! own `ExecStart`, so it is a packaging failure that one restart makes obvious.

mod admin;
mod args;
mod authstore;
mod boot;
mod config;
mod hostinfo;
mod logctl;
mod notify;
mod recover;
mod signals;
mod spool;
mod view;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    // `args_os`, not `args`: the latter panics mid iteration on an argument that is not
    // valid Unicode, and `--config` is a path.
    let args = match args::parse(std::env::args_os().skip(1)) {
        Ok(args::Parsed::Run(args)) => args,
        Ok(args::Parsed::Printed) => return ExitCode::SUCCESS,
        // W5 §3.6's recovery commands. They run **instead of** the daemon and before any
        // subscriber exists, which is why their own output goes to the console directly and
        // why a failure here is reported through `args::fatal` rather than through
        // `tracing`: there is nothing to log with.
        Ok(args::Parsed::Command(command)) => {
            return match recover::run(&command).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    args::fatal(&format!("{e:#}"));
                    ExitCode::FAILURE
                }
            }
        }
        Err(e) => {
            args::fatal(&e.to_string());
            args::usage();
            return ExitCode::from(2);
        }
    };
    match boot::run(&args).await {
        Ok(_report) => ExitCode::SUCCESS,
        Err(e) => {
            // Whichever channel exists, and not both. Every failure after step 2 has a
            // subscriber, so the chain goes through `tracing` and reaches the journal in
            // the configured format; step 2 *itself* failing is the one case with no
            // subscriber, and a daemon that exits 1 silently is what `Type=notify` was
            // adopted to stop. `has_been_set` is the question that distinguishes them.
            let chain = mw_types::DisplayChain(e.as_ref());
            if tracing::dispatcher::has_been_set() {
                tracing::error!(err = %chain, "modernweather stopped");
            } else {
                args::fatal(&chain.to_string());
            }
            ExitCode::FAILURE
        }
    }
}
