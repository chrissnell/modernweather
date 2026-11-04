//! The fan-out: `ReadingStream` over the spool, one subscription shape per policy.
//!
//! **No policy can block a source, and none of the three is on `admit`'s path** (W3 §2.4).
//! `admit` returns once the append is committed; the cursors, the watch and the broadcast
//! are all read *from* the spool by their own tasks. That is the structural answer to the
//! four defects W3 §2.4 measures in `internal/managers/storage.go:212` — a blocking send
//! per engine, with no `select`, no context check, an unsynchronised `s.Engines`, and a
//! `RemoveEngine` that closes a channel the distributor may be sending on.
//!
//! | Policy | Mechanism | On a slow consumer | Why that is right for this kind |
//! |---|---|---|---|
//! | `Durable` | spool cursor | falls behind **on disk**; `mw_spool_lag_envelopes{cursor}` rises; nothing upstream notices | A reading lost from the database is history that cannot be recovered, and **D-4** makes raw permanent, which only means something if raw is complete. |
//! | `Latest` | `watch` | intermediate values are **dropped by design** | The `/latest` payload is replaced every ~2 s. An intermediate value nobody read is not a loss; serialising it would be the waste. |
//! | `Lossy` | `broadcast(1024)` | oldest dropped, `mw_stream_lagged_total{component}` names the subscriber | A sampled metric costs a pixel. A cursor and a segment for data whose value expires in seconds would be the wrong trade. |
//!
//! # A filtered envelope advances the cursor (W7 **D-9**)
//!
//! This is load-bearing and easy to get wrong. A cursor that stalls on an envelope it will
//! never send stalls *forever*: the spool never reclaims, the depth rises, and `max_lag`
//! eventually abandons the cursor — so a mutual uplink would present as a disk filling up.
//!
//! The mechanism is that [`Read::through`] is the highest local position the read
//! *covered*, not the position of the last envelope that survived the filter. `ack()`
//! advances to `through`, so a batch that was entirely filtered advances the cursor by its
//! whole width and the next `next_batch` keeps going. A filtered envelope is "durably
//! handled" in `Sink::deliver`'s sense, exactly as W7 §1.4 says.

use std::sync::Arc;

use mw_component::{
    BoxFuture, ComponentId, ReadingBatch, ReadingFilter, ReadingPolicy, ReadingStream,
    ReadingSubscription, StreamError,
};
use mw_types::{Envelope, Seq};
use tokio::sync::{broadcast, watch};
use tracing::warn;

use crate::metrics as m;
use crate::spool::{Cursor, Spool};

/// The reading fan-out, over one spool.
///
/// A **root** capability: it exists before any component, because it is a root resource the
/// binary owns (W3 §3.1).
///
/// # It holds no shutdown token, and that is a decision
///
/// `ReadingSubscription::next_batch`'s doc says it returns `None` "when the stream has
/// ended — which happens at shutdown", and the obvious way to do that is to hand the stream
/// the process's `CancellationToken`. **That is wrong, and it was measured as wrong**: the
/// supervisor stops components in reverse topological order by cancelling each one's own
/// token (§3.6), and `mw_supervisor::task::run_on_reading` already awaits `next_batch`
/// inside a `select!` against `cx.cancelled()`. A stream that also watched the global token
/// would end every subscription the instant it fired — racing the ordered stop, so a
/// component could exit on its own before the supervisor reached it and be counted as
/// neither stopped nor aborted.
///
/// So stopping stays entirely the supervisor's, which is **D-24**: stopping is cancellation
/// plus ownership, not a method — and not two mechanisms. A subscription ends on its own
/// only for the reasons that are *its* business: an abandoned cursor, or a spool that
/// cannot be read.
#[derive(Debug)]
pub struct SpoolStream {
    spool: Arc<Spool>,
}

impl SpoolStream {
    /// The fan-out over `spool`.
    pub fn new(spool: Arc<Spool>) -> Self {
        Self { spool }
    }

    /// The spool behind it.
    pub fn spool(&self) -> &Arc<Spool> {
        &self.spool
    }
}

impl ReadingStream for SpoolStream {
    fn subscribe(
        &self,
        subscriber: &ComponentId,
        filter: &ReadingFilter,
        policy: ReadingPolicy,
    ) -> Result<Box<dyn ReadingSubscription>, StreamError> {
        let filter = filter.clone();
        match policy {
            ReadingPolicy::Durable => {
                let cursor = self.spool.open_cursor(subscriber.clone()).map_err(|e| {
                    // `CursorBusy` is a plan-level mistake that reached run time -- two
                    // components with one id -- and a spool failure is fatal. Both are
                    // reported through `Backend`, whose own doc says the judgement of
                    // which is which belongs to the stream implementation, and whose
                    // classification is read from the source error.
                    StreamError::backend(e)
                })?;
                Ok(Box::new(DurableSubscription {
                    cursor,
                    filter,
                    in_flight: None,
                }))
            }
            ReadingPolicy::Latest => Ok(Box::new(LatestSubscription {
                rx: self.spool.subscribe_latest(),
                filter,
            })),
            ReadingPolicy::Lossy => Ok(Box::new(LossySubscription {
                subscriber: subscriber.clone(),
                rx: self.spool.subscribe_lossy(),
                filter,
            })),
        }
    }
}

/// Whether one envelope is for one subscriber.
///
/// `ReadingFilter::NotFrom(origin)` is `!prov.has_visited(origin)` and nothing else (W3
/// §2.2). It is also why the uplink's filter is `NotFrom(peer)` and not `NotFrom(self)`
/// (**D-47**): `admit` sets `path = [self]`, so every envelope in a node's own spool
/// carries `self` and `NotFrom(self)` would match nothing.
fn matches(filter: &ReadingFilter, envelope: &Envelope) -> bool {
    match filter {
        ReadingFilter::All => true,
        ReadingFilter::Stations(set) => set.contains(&envelope.station),
        ReadingFilter::NotFrom(origin) => !envelope.provenance.has_visited(*origin),
    }
}

/// A `Durable` subscriber: a spool cursor, and the one place an offset advances.
#[derive(Debug)]
struct DurableSubscription {
    cursor: Cursor,
    filter: ReadingFilter,
    /// The local position the batch currently being handled covers. `ack()` advances to
    /// it; it is **not** derived from the batch's contents, because a filtered envelope has
    /// to advance it too (W7 **D-9**).
    in_flight: Option<Seq>,
}

impl DurableSubscription {
    async fn next(&mut self) -> Option<ReadingBatch> {
        loop {
            if self.cursor.is_abandoned() {
                // `max_lag` fired: the segments this cursor wanted are reclaimed, so
                // waiting produces nothing. Ending the stream puts the component in
                // `Finished` rather than leaving a task parked forever; W3 §2.7's
                // `Failed { CursorAbandoned }` is the state the supervisor should show,
                // and `mw_spool_cursor_state{state="abandoned"}` is what says so until the
                // management API can.
                warn!(
                    cursor = %self.cursor.id(),
                    "the cursor was abandoned past [spool].max_lag, so this subscription \
                     ends. Nothing it had not acknowledged will be delivered."
                );
                return None;
            }
            let read = match self.cursor.read() {
                Ok(read) => read,
                Err(e) => {
                    // A read failure is the spool failing, which is fatal for the process
                    // (W2a Decision 62). Ending the subscription is what reports it: the
                    // task exits, the supervisor fails the component, and the chain is in
                    // the log.
                    warn!(
                        cursor = %self.cursor.id(),
                        err = %mw_types::DisplayChain(&e),
                        "the spool could not be read for this cursor"
                    );
                    return None;
                }
            };
            if read.envelopes.is_empty() {
                // Parked until something is appended. Cancel-safe: this future holds
                // nothing and has taken nothing from a queue, so the supervisor dropping it
                // at a stop loses no envelope -- which is the whole of what the trait asks.
                self.cursor.wait_for_records().await;
                continue;
            }
            self.in_flight = Some(read.through);
            let kept: Vec<Envelope> = read
                .envelopes
                .into_iter()
                .map(|(_, e)| e)
                .filter(|e| matches(&self.filter, e))
                .collect();
            if kept.is_empty() {
                // Every envelope in the window was filtered. Advance the cursor over them
                // rather than returning an empty batch: a cursor that stalls on envelopes
                // it will never send stalls forever (W7 D-9).
                if let Err(e) = self.cursor.ack(read.through) {
                    warn!(
                        cursor = %self.cursor.id(),
                        err = %mw_types::DisplayChain(&e),
                        "the cursor could not be advanced over a fully-filtered window"
                    );
                    return None;
                }
                self.in_flight = None;
                continue;
            }
            return Some(ReadingBatch::new(kept));
        }
    }
}

impl ReadingSubscription for DurableSubscription {
    fn next_batch(&mut self) -> BoxFuture<'_, Option<ReadingBatch>> {
        // Cancel-safe: nothing is taken from a queue. A dropped future leaves the cursor's
        // durable position where it was, and `rewind` puts the read position back, so the
        // same records are read again.
        Box::pin(async move {
            let batch = self.next().await;
            if batch.is_none() {
                self.cursor.rewind();
            }
            batch
        })
    }

    fn ack(&mut self) -> BoxFuture<'_, Result<(), StreamError>> {
        Box::pin(async move {
            let Some(through) = self.in_flight.take() else {
                // Nothing in flight. Not an error: the supervisor acknowledges only a
                // batch it was given, so this can only be reached by a double ack, and
                // advancing something on a guess is worse than doing nothing.
                return Ok(());
            };
            self.cursor.ack(through).map_err(StreamError::backend)
        })
    }
}

/// A `Latest` subscriber: a coalescing watch, capacity 1 by construction.
#[derive(Debug)]
struct LatestSubscription {
    rx: watch::Receiver<Option<Arc<Envelope>>>,
    filter: ReadingFilter,
}

impl ReadingSubscription for LatestSubscription {
    fn next_batch(&mut self) -> BoxFuture<'_, Option<ReadingBatch>> {
        Box::pin(async move {
            loop {
                if self.rx.changed().await.is_err() {
                    // The spool is gone, which at this point means the process is on its
                    // way out: `main` owns it and drops it after every component is stopped.
                    return None;
                }
                let newest = self.rx.borrow_and_update().clone();
                let Some(newest) = newest else { continue };
                if !matches(&self.filter, &newest) {
                    continue;
                }
                return Some(ReadingBatch::new(vec![(*newest).clone()]));
            }
        })
    }

    fn ack(&mut self) -> BoxFuture<'_, Result<(), StreamError>> {
        // No cursor, so nothing to advance. `Latest` is a snapshot, not a log.
        Box::pin(async { Ok(()) })
    }
}

/// A `Lossy` subscriber: a 1024-deep broadcast, oldest dropped and counted.
#[derive(Debug)]
struct LossySubscription {
    subscriber: ComponentId,
    rx: broadcast::Receiver<Arc<Envelope>>,
    filter: ReadingFilter,
}

impl ReadingSubscription for LossySubscription {
    fn next_batch(&mut self) -> BoxFuture<'_, Option<ReadingBatch>> {
        Box::pin(async move {
            loop {
                match self.rx.recv().await {
                    Ok(envelope) if matches(&self.filter, &envelope) => {
                        return Some(ReadingBatch::new(vec![
                            Arc::try_unwrap(envelope).unwrap_or_else(|e| (*e).clone())
                        ]))
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Which subscriber fell behind, by how much. A subscriber more than
                        // 1024 envelopes behind -- about 8.5 minutes at the measured rate
                        // -- is broken, and this is what says which one.
                        m::stream_lagged(&self.subscriber, n);
                        warn!(
                            component = %self.subscriber,
                            dropped = n,
                            "a Lossy subscriber fell more than the broadcast's depth \
                             behind and lost {n} envelopes"
                        );
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
    }

    fn ack(&mut self) -> BoxFuture<'_, Result<(), StreamError>> {
        Box::pin(async { Ok(()) })
    }
}
