//! The reading store's **write** half: the handle `writer:timescale` resolves.
//!
//! # Why this is a separate trait from the read side, and why it shares its capability
//!
//! W3 §2a.1 names three shapes over **one** `sqlx::PgPool` — `TimescaleSink` (write),
//! `ReadingStore` (read) and the fan-out tap (live) — and says they are "named separately
//! so nobody makes the website a sink" (**D-30**). The separation that does the work is
//! the one in the types: `api:legacy` is handed `LatestSnapshot` and never a store handle,
//! so it *cannot* query (**D-35**), and nothing that serves bytes can reach [`ReadingSink`]
//! because nothing that serves bytes declares `Capability::ReadingStore`.
//!
//! It shares `Capability::ReadingStore` with the read half for the same reason
//! [`crate::Ingress`] shares `Capability::ReadingStream` with [`crate::ReadingStream`]: it
//! is the same resource seen from the other end, and `Access::Read` versus `Access::Write`
//! is what distinguishes the two consumers. W2a §4.1b writes `writer:timescale`'s
//! requirement as `ReadingStore` **H W** — hard, write — which is that distinction
//! declared rather than inferred.
//!
//! **What the next item has to settle, stated rather than discovered.** `db:postgres`
//! publishes exactly one handle per capability, and today that handle is
//! `Arc<dyn ReadingSink>`. The read half (`span`, `latest`, `window`, `almanac` — W3
//! §2a.1) arrives with the items that query it: `window:rollup` (M3-4), `snapshot:warm`
//! (M4-1) and `/span` (M4-2). Whichever of those lands first has to choose between two
//! shapes, and both are in the design:
//!
//! 1. one published handle offering both halves, each still its own trait, which keeps
//!    `Capability::ReadingStore` as "the pool is usable"; or
//! 2. the write half travelling outside the capability bus, which is what `Ingress` does
//!    today — `main` hands it to the configuration rather than publishing it (`mw_station`'s
//!    `config` module records why).
//!
//! This item does not pick for them, because the choice depends on the read side's query
//! set and only the item that writes those queries knows it. What it does do is refuse to
//! let the two merge into one trait with eight methods, which is the thing **D-30** is
//! against.

use std::sync::Arc;

use mw_types::Envelope;

use crate::body::BoxFuture;
use crate::capability::Capability;
use crate::cx::CapabilityHandle;

/// What one `store` call did.
///
/// `stored` is the length of the **prefix** that is durably in the database, not a count
/// of successes: a `Sink` must be able to say "the first 100 of these 256 are in", and a
/// count with holes in it is not a position a cursor can take (`mw_pipeline::Acked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stored {
    /// How many envelopes, from the start of the batch, are durable.
    pub stored: usize,
    /// How many rows the statement actually wrote.
    ///
    /// Lower than `stored` when two envelopes in one batch carry the same
    /// `(station_id, time)`: the unique index means the second would overwrite the first,
    /// so they are collapsed before the statement runs. Counted rather than silent,
    /// because a station whose every reading collapses is a station whose observations are
    /// one row per batch and nothing else would say so.
    pub rows: u64,
}

/// The write half of the reading store (W3 §2a.1's `TimescaleSink`).
///
/// One method, because a handle trait is the query set its consumer writes and
/// `writer:timescale` writes exactly one query. **D-56**'s projections are part of it
/// rather than a second method: `stations.last_seen` and `station_metrics` are maintained
/// by the same component, in the same transaction as the rows they describe, so a caller
/// cannot store readings and forget them.
pub trait ReadingSink: Send + Sync + 'static {
    /// Store a batch in order, returning how much of it is durable.
    ///
    /// **Idempotent over a replayed prefix**, which for `writer:timescale` is
    /// `ON CONFLICT (station_id, time) DO UPDATE` — the property W3 §2.3 requires of every
    /// durable sink, because a cancelled delivery replays whatever prefix it had already
    /// written.
    ///
    /// **An unreported metric is `NULL`, never `0.0`** (**D-26**). The frozen contract's
    /// zeros are produced one layer later, at the DTO (§1.7).
    fn store<'a>(&'a self, batch: &'a [Envelope]) -> BoxFuture<'a, Result<Stored, StoreError>>;
}

impl CapabilityHandle for Arc<dyn ReadingSink> {
    /// The read half's capability, because it is the same pool seen from the other end —
    /// exactly as [`crate::Ingress`] and [`crate::ReadingStream`] share
    /// `Capability::ReadingStream`. `Access` is what distinguishes them, not the
    /// capability.
    const CAPABILITY: Capability = Capability::ReadingStore;
}

/// Why a store call failed.
///
/// Classified here rather than at the call site (§4.2), because what happens next differs:
/// a connection that died is a batch that replays, and a statement the server rejected is
/// a build that disagrees with its schema and will reject the same batch forever.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database is unreachable, or the connection died mid-statement.
    ///
    /// The cursor does not advance, so the batch replays; the component goes to `Backoff`
    /// and `mw_spool_lag_envelopes` rises, which is the visible-and-lossless shape W3 §2.3
    /// specifies.
    #[error("the weather database could not be reached")]
    Unavailable {
        /// The cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The server rejected the statement, or a value would not encode.
    ///
    /// Fatal: a column that is not there is not there on the next attempt either, and
    /// retrying it would turn a build/schema mismatch into an unbounded backoff with
    /// nothing in the log that names the cause once.
    #[error("the weather database rejected the write")]
    Rejected {
        /// The cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl mw_types::Classify for StoreError {
    fn class(&self) -> mw_types::Class {
        match self {
            StoreError::Unavailable { .. } => mw_types::Class::RETRYABLE,
            StoreError::Rejected { .. } => mw_types::Class::Fatal,
        }
    }
}

impl StoreError {
    /// A transient failure: the batch replays.
    pub fn unavailable<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        StoreError::Unavailable {
            source: Box::new(source),
        }
    }

    /// A permanent failure: this build and this schema disagree.
    pub fn rejected<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        StoreError::Rejected {
            source: Box::new(source),
        }
    }
}

/// A failed write is a failed occurrence, with the action named (§5.9).
///
/// `mw_pipeline::Sink` requires it: the adapter turns a sink's own error into the
/// supervisor's vocabulary, and the conversion belongs with the error rather than at the
/// one call site, so a second sink over the same store cannot word it differently.
impl From<StoreError> for crate::error::ComponentError {
    fn from(e: StoreError) -> Self {
        crate::error::ComponentError::tick("store a batch of readings", e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_types::{Class, Classify};

    #[test]
    fn a_dead_connection_replays_and_a_rejected_statement_does_not() {
        assert_eq!(
            StoreError::unavailable(std::io::Error::other("connection reset")).class(),
            Class::RETRYABLE
        );
        assert_eq!(
            StoreError::rejected(std::io::Error::other("column \"snowdepth\" does not exist"))
                .class(),
            Class::Fatal
        );
    }

    #[test]
    fn a_store_failure_keeps_its_chain() {
        let e = StoreError::unavailable(std::io::Error::other("no route to host"));
        assert_eq!(
            mw_types::DisplayChain(&e).to_string(),
            "the weather database could not be reached: no route to host"
        );
    }
}
