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
//!
//! # The stack, and why the order is not cosmetic
//!
//! ```text
//! registry()
//!     .with(DedupeLayer)            // event_enabled -> whole-pipeline suppression
//!     .with(reload::Layer<EnvFilter>)   // the global level, runtime-adjustable
//!     .with(WriteLayer)             // stderr: pretty+ANSI on a TTY, JSON otherwise
//!     .with(RingLayer::app.filtered(..))      // the UI buffer, independently filterable
//!     .with(RingLayer::access)      // target == "mw::access" -> the access ring
//! ```
//!
//! [`dedupe::DedupeLayer`] is the only one whose position carries no requirement, and for
//! a reason worth writing down rather than leaving as a coincidence: `Layered` **ANDs**
//! `event_enabled` across the whole stack, so a `false` from any position suppresses the
//! event for every sink. That is the only hook with whole-pipeline reach, and it is why
//! repetition control is a layer rather than a feature of each sink.
//!
//! The one interaction worth knowing: the global filter is a floor for the per-layer ones.
//! An event the reload filter rejects never reaches the ring, however wide the ring's own
//! filter is -- so "raise `mw_station::davis` to DEBUG for ten minutes" (§5.5) raises the
//! *global* directive, and the ring's filter narrows from there rather than widening. The
//! access ring has no filter of its own for the same reason: it sits under the global
//! floor, so a global directive of `warn` empties it, and §5.7's sizing argument assumes
//! it fills.
//!
//! **§5.2's `ring_layer.with_filter(ring_filter)` is spelled differently here, and the
//! difference is a defect it would otherwise introduce.** `Filtered` hands the wrapped
//! layer a `Context` carrying its own `FilterId`, so `Context::event_scope` yields only the
//! spans *that* filter enabled -- and a ring filter narrower than a span's level then
//! produces records with no `station` and no span chain, while the unfiltered stderr layer
//! keeps both. [`RingLayer`] therefore owns its filter and consults it in `on_event`; the
//! reasoning and the one consequence (span-field directives need the per-layer machinery)
//! are on that type.
//!
//! # What is deliberately not here
//!
//! - **The `metrics_layer` of §5.2's sketch** -- "counter events -> the Prometheus
//!   recorder". Nothing in the workspace emits a counter as a `tracing` event: §5.10's
//!   decision is that metrics go through the `metrics` facade, and every counter in this
//!   crate already does. A layer that bridged them would have to invent the field
//!   convention it bridged, with no producer to check it against, so it belongs to the
//!   first crate that wants one.
//! - **The live log subscription.** [`ring::LogRings::merged`] is §5.7's backfill; the SSE
//!   stream that follows it needs a broadcast channel, which needs `tokio`, which
//!   `ci/layers.toml` forbids this crate. It belongs with the management API that serves
//!   it (implementation-plan.md M7-1).
//! - **Both halves of §5.4's "reloadable pair"** -- the field predicate (a `FilterFn` over
//!   a station id) *and* the reloadability. The ring's filter is fixed at startup here;
//!   `install` returns no handle for it, because the thing that would call that handle is
//!   the management route that offers the operator a station to filter by
//!   (implementation-plan.md M7-1). The global directive is reloadable today, which is the
//!   half that has a consumer.
//! - **The TTL on a scoped level raise** (§5.5). The expiry belongs in the supervisor's
//!   timer heap because there is one clock for the whole process (§3.3), and the API that
//!   offers the TTL is M7-1's.

mod dedupe;
mod error;
mod format;
mod level;
mod metrics;
mod record;
mod render;
mod ring;
mod span;
mod stderr;
mod write;

use std::sync::Arc;

use tracing_subscriber::prelude::*;
use tracing_subscriber::reload;

pub use crate::dedupe::{
    Dedupe, DedupeLayer, Policy, Summary, AUDIT_TARGET, DEDUPE, DEDUPE_TARGET,
};
pub use crate::error::ObsError;
pub use crate::format::{Requested, Shape};
pub use crate::level::{parse, Reload};
pub use crate::metrics::{Metrics, CONTENT_TYPE};
pub use crate::record::{Record, Stream, Value};
pub use crate::render::{json, pretty};
pub use crate::ring::{LogRings, LogTap, Ring, RingLayer, ACCESS_TARGET};
pub use crate::stderr::console_banner;
pub use crate::write::WriteLayer;

/// The default filter directive, when neither the operator nor `RUST_LOG` says otherwise.
///
/// `info` for this program and `warn` for its dependencies: `sqlx` logs every statement at
/// `debug` and `hyper` logs every connection, and the first thing an operator would have
/// to do on a default install is turn them down.
pub const DEFAULT_DIRECTIVE: &str = "info,sqlx=warn,hyper=warn,hyper_util=warn,rustls=warn";

/// How the binary asks for the stack it wants.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The global filter directive. Empty means [`DEFAULT_DIRECTIVE`].
    pub directive: Option<String>,
    /// The ring's own filter directive (§5.2: "independently filterable"). `None` means
    /// the same as the global one.
    pub ring_directive: Option<String>,
    /// An explicit `--log-format`, which beats the detection of §5.3.
    pub format: Option<Requested>,
}

/// Everything the binary keeps after the subscriber is installed.
#[derive(Debug)]
pub struct Obs {
    log: Arc<Reload>,
    rings: Arc<LogRings>,
    dedupe: Arc<Dedupe>,
    metrics: Metrics,
    shape: Shape,
    directive: String,
}

impl Obs {
    /// The log-level handle. The binary implements `mw_component::LogControl` over this
    /// and hands it to the supervisor as a root fact (**D-52**).
    pub fn log(&self) -> Arc<Reload> {
        Arc::clone(&self.log)
    }

    /// The UI's buffers (§5.7).
    pub fn rings(&self) -> Arc<LogRings> {
        Arc::clone(&self.rings)
    }

    /// The metrics handle, which `/metrics` renders from.
    pub fn metrics(&self) -> Metrics {
        self.metrics.clone()
    }

    /// The format in force, for the startup line that says which one was chosen.
    pub fn shape(&self) -> Shape {
        self.shape
    }

    /// The filter directive the process started with.
    pub fn directive(&self) -> &str {
        &self.directive
    }

    /// Everything that has to happen periodically, in one call.
    ///
    /// Two jobs, both of which §3.3 says must not grow a timer of their own: emitting the
    /// dedupe summaries (§5.6) and expiring the exporter's idle histogram buckets. The
    /// binary's loop tick calls this.
    pub fn tick(&self) {
        self.dedupe.sweep();
        self.metrics.upkeep();
    }
}

/// Install the subscriber stack and the metrics recorder.
///
/// Once per process: both are process-global, and a second call is reported rather than
/// silently ignored.
pub fn install(options: &Options) -> Result<Obs, ObsError> {
    let directive = options
        .directive
        .clone()
        .or_else(|| std::env::var("RUST_LOG").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| DEFAULT_DIRECTIVE.to_owned());
    let shape = stderr::shape(options.format).map_err(|value| ObsError::FormatUnknown {
        value,
        expected: Requested::SPELLINGS,
    })?;

    let rings = Arc::new(LogRings::new());
    let dedupe = Arc::new(Dedupe::new(DEDUPE));
    let ring_filter = parse(options.ring_directive.as_deref().unwrap_or(&directive))?;
    let (filter_layer, handle) = reload::Layer::new(parse(&directive)?);

    // Before the subscriber, so that a `log` record emitted during installation by a
    // dependency is not the one thing that escapes the pipeline. `try_init` would do this
    // for us and with a max-level hint, but it also sets the global default, and doing
    // both in one call leaves no way to report which half failed.
    tracing_log::LogTracer::init().map_err(|e| ObsError::SubscriberInstalled {
        reason: e.to_string(),
    })?;

    let metrics = Metrics::install()?;
    metrics.register_own();

    let subscriber = tracing_subscriber::registry()
        .with(DedupeLayer::new(Arc::clone(&dedupe)))
        .with(filter_layer)
        .with(WriteLayer::new(shape))
        .with(RingLayer::app(Arc::clone(&rings)).filtered(ring_filter))
        .with(RingLayer::access(Arc::clone(&rings)));
    tracing::subscriber::set_global_default(subscriber).map_err(|e| {
        ObsError::SubscriberInstalled {
            reason: e.to_string(),
        }
    })?;

    Ok(Obs {
        log: Arc::new(Reload::new(handle, &directive)),
        rings,
        dedupe,
        metrics,
        shape,
        directive,
    })
}
