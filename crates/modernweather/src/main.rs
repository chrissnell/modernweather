//! The ModernWeather daemon.
//!
//! The only crate that may depend on all the others, and the only one that may depend on
//! `anyhow` (docs/service-architecture.md §1.2, §4). It is where a capability consumer and
//! its provider meet: the binary resolves handles, calls `plan()` and runs the supervisor,
//! which is what lets every other crate name a trait instead of an implementation (§1.3).
//!
//! A stub. `--config <path>`, the SQLite pool as a root resource owned by `main`, the
//! subscriber and metrics recorder, the database-version assertion and `sd-notify`
//! readiness are implementation-plan.md M2-4; `plan()` itself is M2-3.

fn main() {}
