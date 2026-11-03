//! Metrics are not logs (§5.10).
//!
//! **Prometheus text exposition on an additive `/metrics` route on the one listener**, via
//! the `metrics` facade and `metrics-exporter-prometheus`. Why Prometheus rather than
//! counters on a status endpoint: a status endpoint has to invent a format, and the
//! operator already runs a homelab where a scraper is one more target. It costs nothing
//! when nobody scrapes -- the recorder is an in-process registry and the route renders on
//! demand.
//!
//! **The exporter's own HTTP listener is not linked.** `metrics-exporter-prometheus` is
//! pinned `default-features = false` in `[workspace.dependencies]` precisely so that the
//! `http-listener` feature is off: only `mw-http` may bind a socket (§1.4), and a metrics
//! exporter quietly binding its own port would be the seventh listener-owning site this
//! design exists to remove. So this type renders a `String` and `mw-http` serves it.
//!
//! **Cardinality is bounded by construction and that is a rule, not an observation**:
//! every label is a station id, a component id, a route pattern or a closed enum. A label
//! whose values come from a request -- a path, a `Host`, a user agent -- is forbidden, and
//! the review rule is that a new label must be justifiable as "there are N of these and N
//! is in the configuration".

use std::sync::Arc;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle, PrometheusRecorder};

use crate::error::ObsError;

/// The content type Prometheus text exposition is served as.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// A handle to the installed recorder.
#[derive(Clone, Debug)]
pub struct Metrics {
    handle: PrometheusHandle,
    /// Set only by [`Metrics::detached`]. The installed recorder is owned by `metrics`
    /// itself and lives for the process; a detached one has to be kept alive here, because
    /// the handle reads the registry the recorder owns.
    _detached: Option<Arc<PrometheusRecorder>>,
}

impl Metrics {
    /// Install the process-global recorder.
    ///
    /// Once per process: `metrics::set_global_recorder` refuses a second one, and a second
    /// call is a programming error rather than a condition -- so it is reported as
    /// [`ObsError::RecorderInstalled`] and not retried.
    pub fn install() -> Result<Self, ObsError> {
        let handle = PrometheusBuilder::new().install_recorder().map_err(|e| {
            ObsError::RecorderInstalled {
                reason: e.to_string(),
            }
        })?;
        Ok(Metrics {
            handle,
            _detached: None,
        })
    }

    /// A recorder that is **not** installed globally, for a test that needs a handle
    /// without claiming the process's only recorder slot.
    pub fn detached() -> Self {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        Metrics {
            handle,
            _detached: Some(Arc::new(recorder)),
        }
    }

    /// The current exposition payload, rendered on demand.
    pub fn render(&self) -> String {
        self.handle.render()
    }

    /// Describe and zero the counters this crate owns.
    ///
    /// A Prometheus counter does not exist in the exposition until something touches it,
    /// so a daemon that has never dropped a log record and a daemon whose metrics are
    /// broken look identical to a scraper -- and an alert on `rate(...)` over a series
    /// that appears halfway through a window reads as a step change rather than as the
    /// first increment. Zeroing them at startup makes "nothing has gone wrong yet" an
    /// assertion rather than an absence.
    pub fn register_own(&self) {
        metrics::describe_counter!(
            "mw_ring_dropped_total",
            "Log records evicted from a UI ring buffer before anything read them"
        );
        for stream in [crate::Stream::App, crate::Stream::Access] {
            metrics::counter!("mw_ring_dropped_total", "stream" => stream.as_str()).absolute(0);
        }
        metrics::describe_counter!(
            "mw_dedupe_evicted_total",
            "Repetition-control keys evicted because the bounded table was full"
        );
        metrics::counter!("mw_dedupe_evicted_total").absolute(0);
    }

    /// Expire idle histogram buckets.
    ///
    /// `install_recorder` makes this the caller's job, and the caller is the binary's loop
    /// tick -- there is one clock for the whole process (§3.3), so the exporter does not
    /// get a timer of its own.
    pub fn upkeep(&self) {
        self.handle.run_upkeep();
    }
}
