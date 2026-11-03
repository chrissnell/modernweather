//! The `ReadingStream` capability handle: how a batch reaches an `OnReading` component.
//!
//! **A component does not subscribe.** It declares `Trigger::OnReading { filter, policy,
//! deadline }` and the supervisor delivers batches to [`crate::OnReading::readings`]. No
//! component holds a channel receiver, which is what makes the delivery policy a
//! supervisor concern rather than N local decisions (W2a §5).
//!
//! The implementation of this trait is **M3-2's**: `Durable` requires a local durable
//! spool at the fan-out point, because the stream never blocks a source and a bounded
//! in-memory queue would therefore mean loss while an unbounded one would mean an OOM on a
//! stalled database (W2a Decision 62). What is fixed here is only the boundary: what the
//! supervisor asks for, and what acknowledgement means.

use std::sync::Arc;

use crate::body::{BoxFuture, ReadingBatch};
use crate::capability::Capability;
use crate::cx::CapabilityHandle;
use crate::id::ComponentId;
use crate::trigger::{ReadingFilter, ReadingPolicy};

/// The live reading fan-out. A root capability: it exists before any component.
pub trait ReadingStream: Send + Sync + 'static {
    /// Open a subscription for one component.
    ///
    /// The supervisor is the only caller. The filter and policy come from the component's
    /// declared trigger, so "what do I receive and what may be lost" is a property of the
    /// plan rather than of a call site.
    fn subscribe(
        &self,
        subscriber: &ComponentId,
        filter: &ReadingFilter,
        policy: ReadingPolicy,
    ) -> Result<Box<dyn ReadingSubscription>, StreamError>;
}

impl CapabilityHandle for Arc<dyn ReadingStream> {
    const CAPABILITY: Capability = Capability::ReadingStream;
}

/// One component's view of the stream.
pub trait ReadingSubscription: Send {
    /// The next batch, or `None` when the stream has ended — which happens at shutdown.
    ///
    /// Must be cancel-safe: the supervisor awaits this inside a `select!` against
    /// cancellation, and a batch that was taken from the queue by a future that is then
    /// dropped would be a batch lost without being acknowledged.
    fn next_batch(&mut self) -> BoxFuture<'_, Option<ReadingBatch>>;

    /// Acknowledge the batch last returned by [`ReadingSubscription::next_batch`].
    ///
    /// Called by the supervisor **only** when the component's `readings` returned `Ok`, so
    /// for a `Durable` subscriber the cursor advances exactly once per handled batch and a
    /// failed delivery replays the same batch. For `Latest` and `Lossy` there is no cursor
    /// and this is a no-op.
    fn ack(&mut self) -> BoxFuture<'_, Result<(), StreamError>>;
}

/// Why the stream could not be read or acknowledged.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// The stream exists but cannot serve this subscription — a filter naming a station
    /// that is not configured, or a `Durable` policy with no spool behind it.
    #[error("{subscriber} cannot subscribe: {reason}")]
    Unsupported {
        /// Who asked.
        subscriber: ComponentId,
        /// Why not.
        reason: String,
    },

    /// The backing store failed. For a `Durable` subscriber this is where "if the spool
    /// itself cannot be written, that is fatal for the process, not a silent drop" is
    /// decided — by the stream implementation, which is the only thing that knows whether
    /// the failure was the spool (W2a Decision 62).
    #[error("the reading stream failed")]
    Backend {
        /// The cause.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl mw_types::Classify for StreamError {
    /// `Unsupported` is a plan-level mistake that reached run time, so no retry helps.
    /// `Backend` is transient **here**: whether a failure was the spool — which W2a
    /// Decision 62 makes fatal for the process rather than a silent drop — is a judgement
    /// only the stream implementation can make, so a fatal one is reported as such by
    /// constructing the variant from an error whose own class is `Fatal`. The decision is
    /// recorded beside the implementation, which is §4.2's whole point.
    fn class(&self) -> mw_types::Class {
        match self {
            StreamError::Unsupported { .. } => mw_types::Class::Fatal,
            StreamError::Backend { .. } => mw_types::Class::RETRYABLE,
        }
    }
}

impl StreamError {
    /// Wrap a backend failure.
    pub fn backend<E>(source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        StreamError::Backend {
            source: Box::new(source),
        }
    }
}
