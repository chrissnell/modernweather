//! Observability: one logging path, and a metrics surface that is not logs.
//!
//! The subscriber stack, the `EnvFilter` reload handle, the ring layer, the dedupe layer,
//! the access-log layer and the metrics recorder (docs/service-architecture.md §5). The Go
//! version has a package-global sugared logger *and* an injected one, used
//! interchangeably inside the same functions; neither is reproduced. A component gets its
//! logger as the ambient subscriber plus the span `Cx` carries.
//!
//! `tracing_log::LogTracer` is installed from here so that dependency crates using `log`
//! -- `sqlx`, `hyper`, `rustls` -- land in the same pipeline. Workspace code uses
//! `tracing` macros only, and `ci/layers.toml` forbids any crate from declaring `log`.
//!
//! Forbidden to depend on everything but `mw-types`.
