//! Step 4b: turn the `node` row into an open spool and a reading fan-out.
//!
//! **Why this is in the binary.** The spool is a root resource (W3 §3.1, **D-30a**), and
//! its configuration lives in `mw-store`'s `node` row while its implementation lives in
//! `mw-pipeline` — two crates in the same layer, which `ci/layers.toml` forbids from naming
//! each other. The binary is the only place they can meet, which is §1.3 rule 1 working as
//! intended rather than an inconvenience.
//!
//! **What it deliberately does not build.** `SpoolIngress` — the *write* end — needs a
//! `StationRegistry` handle, whose provider is `config:sqlite` and which does not exist
//! yet. There is also a seam worth knowing about before M3-1 meets it: `Arc<dyn Ingress>`
//! and `Arc<dyn ReadingStream>` both declare `Capability::ReadingStream` (W3 §2.3 writes
//! the station's need as `Need { ReadingStream, Hard, Write }`, so the capability is one
//! and the `Access` is what differs), and the capability bus holds one handle **type** per
//! provider. So the first component that needs the write end will have to be handed it by
//! its registrar, from here, rather than resolving it off the bus — or `Access` has to
//! reach handle resolution. That is M3-1's call against a real consumer, and it is recorded
//! in this item's PR rather than only here.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use mw_pipeline::{Clock, Spool, SpoolConfig, SpoolStream, SystemClock};
use mw_store::config_db::Node;
use mw_types::OriginId;
use tracing::info;

/// The spool and the fan-out over it, held by `main` for the life of the process.
#[derive(Debug)]
pub struct Fanout {
    /// The log itself, for the metrics the boot loop publishes and for whoever ends up
    /// holding the write end.
    pub spool: Arc<Spool>,
    /// The `ReadingStream` root capability.
    pub stream: Arc<SpoolStream>,
}

/// Open the spool described by the `node` row.
pub fn open(node: &Node) -> anyhow::Result<Fanout> {
    let node_id = OriginId::new(<[u8; 16]>::try_from(node.node_id.as_slice()).map_err(|_| {
        anyhow::anyhow!(
            "node.node_id is {} bytes, and a NodeId is 16",
            node.node_id.len()
        )
    })?);
    let ceiling_bytes =
        u64::try_from(node.spool_ceiling_bytes).context("node.spool_ceiling_bytes is negative")?;
    let max_lag = u64::try_from(node.spool_max_lag_seconds)
        .map(Duration::from_secs)
        .context("node.spool_max_lag_seconds is negative")?;

    let cfg = SpoolConfig {
        ceiling_bytes,
        max_lag,
        ..SpoolConfig::new(&node.spool_path)
    };
    // `SystemClock`, because this is the daemon. The seam exists so that `max_lag` --
    // seven days -- is assertable at all (`mw_pipeline::clock`).
    let spool = Spool::open(cfg, node_id, Arc::new(SystemClock) as Arc<dyn Clock>)?;
    info!(
        path = %node.spool_path,
        tail_seq = spool.tail_seq().get(),
        depth_bytes = spool.depth_bytes(),
        ceiling_bytes,
        max_lag_s = max_lag.as_secs(),
        "spool open"
    );
    // No shutdown token: stopping is the supervisor's, in reverse topological order
    // (**D-24**, §3.6). `SpoolStream`'s own doc has the measurement behind that.
    let stream = Arc::new(SpoolStream::new(Arc::clone(&spool)));
    Ok(Fanout { spool, stream })
}
