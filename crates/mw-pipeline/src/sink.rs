//! `Sink`, and the adapter that makes one a `Trigger::OnReading` component.
//!
//! W2a Decision 4 gives the supervisor `readings(batch, cx) -> Result<()>`, which cannot
//! express a partial ack — and a partial ack is exactly what a durable sink needs, so that
//! a cancelled or timed-out delivery replays only what was not stored. So [`Sink`] is a
//! narrower trait and [`SpooledSink`] implements W2a's trait for any of them.
//!
//! # Where the cursor lives, and the one place this diverges from W3 §2.3's sketch
//!
//! §2.3 sketches `SpooledSink<S> { sink: S, cursor: Cursor }` with `readings` calling
//! `self.cursor.ack(n)`. What landed in M2-3 puts the cursor in the **subscription**: the
//! supervisor calls `ReadingSubscription::ack()` itself, and only on a batch the component
//! returned `Ok` for (`mw_supervisor::task::run_on_reading`). Both readings are in the
//! design and only one can be built, because the supervisor is already written.
//!
//! The cursor therefore lives in the subscription and `SpooledSink` holds none. `Acked(n)`
//! with `n < len` is handled **inside one delivery**: the adapter calls `deliver` again with
//! the remaining suffix until the whole batch is handled, so nothing is lost and nothing is
//! delivered twice.
//!
//! **What this does not preserve, stated plainly.** §2.3 says `Acked(0)` with `Ok` "is how
//! a sink expresses soft backpressure **without failing**". Here it fails: the adapter
//! retries inside the occurrence, and a sink that keeps saying "not yet" runs into the
//! trigger's deadline, which `mw_supervisor::task::run_on_reading` turns into
//! `Outcome::DeadlineExceeded` and then `Failed` + `Backoff`. Nothing is lost — the cursor
//! did not advance, so the batch replays — but the component does **not** stay `Running`,
//! and a brief database stall therefore reads as a sink failure rather than as
//! backpressure.
//!
//! That is not fixable here. Expressing it properly needs `ReadingSubscription` to be able
//! to acknowledge a *prefix* (`ack_through(n)`), which is `mw-component`'s trait and
//! `mw-supervisor`'s driver — M2-3's crate, not this item's. Recorded as a known gap rather
//! than papered over, because the alternative on offer was returning `Ok(())` with nothing
//! handled, and that advances the cursor past undelivered records.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mw_component::{BoxFuture, ComponentError, ComponentId, Cx, OnReading, ReadingBatch};
use mw_types::{Classify, Envelope};

use crate::metrics as m;

/// How long the adapter pauses before re-offering a batch a sink handled none of.
///
/// Short, because the bound that matters is the trigger's declared deadline — 5 s for
/// `writer:timescale`, 10 s for an uplink — and this only decides how many times the sink
/// is asked inside it. Long enough that a sink saying "not yet" is not a spin loop.
const SOFT_BACKPRESSURE_PAUSE: Duration = Duration::from_millis(50);

/// How far a delivery got.
///
/// The length of the **prefix** that is durably handled, not a count of successes: a
/// durable sink must be able to say "the first 100 of these 256 are in the database", and a
/// count with holes in it is not a position a cursor can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Acked(pub usize);

/// The delivery side of the pipeline.
pub trait Sink: Send {
    /// What can go wrong, classified beside its own definition (§4.2).
    type Error: Classify + Into<ComponentError> + std::error::Error + Send + Sync + 'static;

    /// Deliver a batch in order, returning the length of the durably-handled prefix.
    ///
    /// Returning `Acked(0)` with `Ok` is legal and means "try again later".
    ///
    /// **Cancellation:** must be cancel-safe. On cancellation mid-batch, whatever prefix
    /// was durably handled is lost from the ack's point of view and simply replays, so a
    /// sink must be idempotent over a replayed prefix — which for `writer:timescale` is
    /// `ON CONFLICT (station_id, time) DO UPDATE`.
    fn deliver<'a>(
        &'a mut self,
        batch: &'a [Envelope],
        cx: &'a Cx,
    ) -> BoxFuture<'a, Result<Acked, Self::Error>>;
}

/// Implements W2a's `OnReading` for any [`Sink`].
///
/// Holds no cursor: see the module doc. What it owns is the partial-ack arithmetic and the
/// counter that makes a partial ack visible.
#[derive(Debug)]
pub struct SpooledSink<S> {
    sink: S,
    id: ComponentId,
}

impl<S: Sink> SpooledSink<S> {
    /// Wrap a sink as an `OnReading` body.
    pub fn new(id: ComponentId, sink: S) -> Self {
        Self { sink, id }
    }

    async fn run(&mut self, batch: ReadingBatch, cx: &Cx) -> Result<(), ComponentError> {
        let started = tokio::time::Instant::now();
        let outcome = self.deliver_all(&batch, cx).await;
        // §5.10's `mw_sink_delivery_seconds`, which is what "distinguishes 'slow' from
        // 'stalled'". Recorded on both outcomes: the interesting samples are the slow ones.
        m::sink_delivery(&self.id, started.elapsed().as_secs_f64());
        outcome
    }

    async fn deliver_all(&mut self, batch: &ReadingBatch, cx: &Cx) -> Result<(), ComponentError> {
        let envelopes = batch.envelopes();
        let mut handled = 0usize;
        let mut partial = false;
        while handled < envelopes.len() {
            let Acked(n) = self
                .sink
                .deliver(&envelopes[handled..], cx)
                .await
                .map_err(Into::into)?;
            if n > envelopes.len() - handled {
                // A sink that claims more than it was given is a bug in that sink, and
                // believing it would advance the cursor past records nobody stored.
                // Clamping would hide it; the whole batch replays instead.
                return Err(ComponentError::tick(
                    "deliver a batch to a durable sink",
                    OverAck {
                        claimed: n,
                        offered: envelopes.len() - handled,
                    },
                ));
            }
            if n == 0 {
                // Counted once per delivery, below, rather than once per 50 ms retry.
                partial = true;
                // Soft backpressure. The trigger's deadline is the bound: the supervisor
                // cancels this occurrence through a child token when it expires, and the
                // batch replays because the cursor has not advanced.
                if cx.is_cancelled() {
                    return Err(ComponentError::Cancelled);
                }
                tokio::time::sleep(SOFT_BACKPRESSURE_PAUSE).await;
                continue;
            }
            if handled + n < envelopes.len() {
                partial = true;
            }
            handled += n;
        }
        if partial {
            m::sink_partial_ack(&self.id, handled, envelopes.len());
        }
        Ok(())
    }
}

impl<S: Sink> OnReading for SpooledSink<S> {
    fn readings<'a>(
        &'a mut self,
        batch: ReadingBatch,
        cx: &'a Cx,
    ) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(self.run(batch, cx))
    }
}

/// A sink claimed a prefix longer than the batch it was offered.
///
/// Its own error type rather than a string, because `Classify` is the thing that decides
/// what happens next and a string has no classification (§4.2). `Fatal`: a sink that
/// miscounts will miscount again, and retrying it would advance a cursor past records
/// nobody stored.
#[derive(Debug, thiserror::Error)]
#[error("the sink acknowledged {claimed} of the {offered} envelopes it was offered")]
pub struct OverAck {
    /// What it claimed.
    pub claimed: usize,
    /// What it was given.
    pub offered: usize,
}

impl Classify for OverAck {
    fn class(&self) -> mw_types::Class {
        mw_types::Class::Fatal
    }
}

/// A sink that records what it was given, for a test that needs one.
///
/// Public rather than `#[cfg(test)]`, because this crate's integration tests are separate
/// compilation units — the same reason `mw_supervisor::testing` is public.
#[derive(Debug, Clone, Default)]
pub struct RecordingSink {
    /// Every envelope delivered, in order.
    pub delivered: Arc<Mutex<Vec<Envelope>>>,
    /// How many times `deliver` was called.
    pub calls: Arc<Mutex<usize>>,
    /// How much of each offered batch to acknowledge. `None` means all of it.
    pub ack_at_most: Option<usize>,
}

impl RecordingSink {
    /// A sink that handles whatever it is given.
    pub fn new() -> Self {
        Self::default()
    }

    /// A sink that handles at most `n` envelopes per call — the partial-ack case.
    pub fn acking_at_most(n: usize) -> Self {
        Self {
            ack_at_most: Some(n),
            ..Self::default()
        }
    }

    /// What it has been given.
    pub fn envelopes(&self) -> Vec<Envelope> {
        self.delivered
            .lock()
            .expect("the recording sink is not poisoned")
            .clone()
    }

    /// How many times it was called.
    pub fn call_count(&self) -> usize {
        *self
            .calls
            .lock()
            .expect("the recording sink is not poisoned")
    }
}

/// A `RecordingSink` never fails, and the trait still needs an error type.
#[derive(Debug, thiserror::Error)]
#[error("a recording sink does not fail")]
pub struct Never;

impl Classify for Never {
    fn class(&self) -> mw_types::Class {
        mw_types::Class::Fatal
    }
}

impl From<Never> for ComponentError {
    fn from(e: Never) -> Self {
        ComponentError::tick("deliver to a recording sink", e)
    }
}

impl Sink for RecordingSink {
    type Error = Never;

    fn deliver<'a>(
        &'a mut self,
        batch: &'a [Envelope],
        _cx: &'a Cx,
    ) -> BoxFuture<'a, Result<Acked, Never>> {
        let take = self.ack_at_most.map_or(batch.len(), |n| n.min(batch.len()));
        self.delivered
            .lock()
            .expect("the recording sink is not poisoned")
            .extend_from_slice(&batch[..take]);
        *self
            .calls
            .lock()
            .expect("the recording sink is not poisoned") += 1;
        Box::pin(async move { Ok(Acked(take)) })
    }
}
