//! Test support: the doubles a supervisor needs before any real component crate exists.
//!
//! Public rather than `#[cfg(test)]`, because this crate's own integration tests are
//! separate compilation units and implementation-plan.md M2-4's startup tests will want
//! the same pieces. Every type here is a **test double with no production use**: the
//! real `ScheduleStore` is `mw-store`'s over `component_schedule`, the real
//! `ReadingStream` is M3-2's spooled fan-out, and the real `LogControl` is `mw-obs`'s
//! reload handle.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mw_component::{
    BoxFuture, Builder, ComponentError, ComponentId, ComponentSpec, Cx, LogControl,
    LogControlError, OccurrenceRecord, ReadingBatch, ReadingFilter, ReadingPolicy, ReadingStream,
    ReadingSubscription, ScheduleStore, ScheduleStoreError, Serve, StreamError, Tick, Trigger,
};
use tokio::sync::mpsc;

use crate::Roots;

/// A [`ScheduleStore`] in memory. `Misfire::RunLatest` across a restart needs a real one;
/// a test that does not restart does not.
#[derive(Debug, Default)]
pub struct MemoryScheduleStore {
    rows: Mutex<BTreeMap<ComponentId, OccurrenceRecord>>,
}

impl MemoryScheduleStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed a recorded occurrence, which is how a test stands in for a restart.
    pub fn seed(&self, record: OccurrenceRecord) {
        self.rows
            .lock()
            .expect("the test store is not poisoned")
            .insert(record.key.component.clone(), record);
    }
}

impl ScheduleStore for MemoryScheduleStore {
    fn latest(
        &self,
        component: &ComponentId,
    ) -> BoxFuture<'_, Result<Option<OccurrenceRecord>, ScheduleStoreError>> {
        let found = self
            .rows
            .lock()
            .expect("the test store is not poisoned")
            .get(component)
            .cloned();
        Box::pin(async move { Ok(found) })
    }

    fn all(&self) -> BoxFuture<'_, Result<Vec<OccurrenceRecord>, ScheduleStoreError>> {
        let all = self
            .rows
            .lock()
            .expect("the test store is not poisoned")
            .values()
            .cloned()
            .collect();
        Box::pin(async move { Ok(all) })
    }

    fn record(&self, record: &OccurrenceRecord) -> BoxFuture<'_, Result<(), ScheduleStoreError>> {
        self.rows
            .lock()
            .expect("the test store is not poisoned")
            .insert(record.key.component.clone(), record.clone());
        Box::pin(async move { Ok(()) })
    }
}

/// A [`LogControl`] that holds a directive and nothing else.
#[derive(Debug)]
pub struct MemoryLogControl {
    directive: Mutex<String>,
    initial: String,
}

impl Default for MemoryLogControl {
    fn default() -> Self {
        Self::new("info")
    }
}

impl MemoryLogControl {
    /// A handle starting at `initial`.
    pub fn new(initial: &str) -> Self {
        Self {
            directive: Mutex::new(initial.to_string()),
            initial: initial.to_string(),
        }
    }
}

impl LogControl for MemoryLogControl {
    fn current(&self) -> String {
        self.directive
            .lock()
            .expect("the test handle is not poisoned")
            .clone()
    }

    fn set(&self, directive: &str) -> Result<(), LogControlError> {
        if directive.is_empty() {
            return Err(LogControlError::FilterParse {
                directive: directive.to_string(),
                reason: "empty".into(),
            });
        }
        *self
            .directive
            .lock()
            .expect("the test handle is not poisoned") = directive.to_string();
        Ok(())
    }

    fn reset(&self) -> Result<(), LogControlError> {
        self.set(&self.initial.clone())
    }
}

/// How deep a subscriber's queue is in this double.
///
/// Twenty, mirroring the Go reading distributor's `make(chan types.Reading, 20)`, because
/// the defect D-29 records is an **unguarded blocking send** into exactly that depth: a
/// stalled TimescaleDB back-pressures the serial read loops and a Davis console that is
/// not drained loses packets. An unbounded queue here would delete the failure mode rather
/// than survive it, and the test asserting "a source is never blocked by a sink" would
/// then be asserting a property of the double.
pub const SUBSCRIBER_DEPTH: usize = 20;

/// A [`ReadingStream`] fed by hand: one bounded sender per subscriber.
#[derive(Debug, Default)]
pub struct ChannelStream {
    subscribers: Mutex<Vec<(ComponentId, mpsc::Sender<ReadingBatch>)>>,
    /// How many batches each subscriber has acknowledged.
    acks: Arc<Mutex<BTreeMap<ComponentId, usize>>>,
    /// How many deliveries each subscriber's full queue refused.
    refused: Arc<Mutex<BTreeMap<ComponentId, usize>>>,
}

impl ChannelStream {
    /// A stream with no subscribers yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer a batch to one subscriber, **without waiting**.
    ///
    /// `try_send`, never `send`: a fan-out that awaited a full queue would be the Go
    /// defect. A refusal is counted and reported, which is what the real fan-out does
    /// instead — it spools (M3-2) or counts a drop, and in neither case does the source
    /// wait. Returns whether the batch was accepted.
    pub fn deliver(&self, subscriber: &ComponentId, batch: ReadingBatch) -> bool {
        let sender = self
            .subscribers
            .lock()
            .expect("the test stream is not poisoned")
            .iter()
            .find(|(id, _)| id == subscriber)
            .map(|(_, tx)| tx.clone());
        match sender {
            Some(tx) => match tx.try_send(batch) {
                Ok(()) => true,
                Err(_) => {
                    *self
                        .refused
                        .lock()
                        .expect("the test stream is not poisoned")
                        .entry(subscriber.clone())
                        .or_default() += 1;
                    false
                }
            },
            None => false,
        }
    }

    /// How many deliveries a subscriber's full queue refused.
    pub fn refused(&self, subscriber: &ComponentId) -> usize {
        self.refused
            .lock()
            .expect("the test stream is not poisoned")
            .get(subscriber)
            .copied()
            .unwrap_or_default()
    }

    /// How many batches a subscriber has acknowledged — the cursor, in miniature.
    pub fn acks(&self, subscriber: &ComponentId) -> usize {
        self.acks
            .lock()
            .expect("the test stream is not poisoned")
            .get(subscriber)
            .copied()
            .unwrap_or_default()
    }
}

impl ReadingStream for ChannelStream {
    fn subscribe(
        &self,
        subscriber: &ComponentId,
        _filter: &ReadingFilter,
        _policy: ReadingPolicy,
    ) -> Result<Box<dyn ReadingSubscription>, StreamError> {
        let (tx, rx) = mpsc::channel(SUBSCRIBER_DEPTH);
        self.subscribers
            .lock()
            .expect("the test stream is not poisoned")
            .retain(|(id, _)| id != subscriber);
        self.subscribers
            .lock()
            .expect("the test stream is not poisoned")
            .push((subscriber.clone(), tx));
        Ok(Box::new(ChannelSubscription {
            subscriber: subscriber.clone(),
            rx,
            acks: Arc::clone(&self.acks),
        }))
    }
}

#[derive(Debug)]
struct ChannelSubscription {
    subscriber: ComponentId,
    rx: mpsc::Receiver<ReadingBatch>,
    acks: Arc<Mutex<BTreeMap<ComponentId, usize>>>,
}

impl ReadingSubscription for ChannelSubscription {
    fn next_batch(&mut self) -> BoxFuture<'_, Option<ReadingBatch>> {
        Box::pin(self.rx.recv())
    }

    fn ack(&mut self) -> BoxFuture<'_, Result<(), StreamError>> {
        *self
            .acks
            .lock()
            .expect("the test stream is not poisoned")
            .entry(self.subscriber.clone())
            .or_default() += 1;
        Box::pin(async { Ok(()) })
    }
}

/// The roots a test supervisor runs on, with a stream.
pub fn roots_with_stream(stream: Arc<ChannelStream>) -> Roots {
    Roots {
        schedule: Arc::new(MemoryScheduleStore::new()),
        stream: Some(stream as Arc<dyn ReadingStream>),
        log: Arc::new(MemoryLogControl::default()),
    }
}

/// The roots a test supervisor runs on, with no stream — the shape `plan()` rejects an
/// `OnReading` component against.
pub fn roots() -> Roots {
    Roots {
        schedule: Arc::new(MemoryScheduleStore::new()),
        stream: None,
        log: Arc::new(MemoryLogControl::default()),
    }
}

/// A capability handle a test can publish, for `ReadingStore`.
///
/// The real one is `mw-store`'s and its trait is the query set M3-3 writes. What a
/// lifecycle test needs is only that *something* typed can be published and resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestReadingStore(pub &'static str);

impl mw_component::CapabilityHandle for TestReadingStore {
    const CAPABILITY: mw_component::Capability = mw_component::Capability::ReadingStore;
}

/// The same, for `WindowedView` — so a test can have a two-link chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestWindowedView;

impl mw_component::CapabilityHandle for TestWindowedView {
    const CAPABILITY: mw_component::Capability = mw_component::Capability::WindowedView;
}

/// A `Tick` body that counts its occurrences and does nothing else.
#[derive(Debug, Default)]
pub struct CountingTick {
    /// How many occurrences have run.
    pub runs: Arc<Mutex<usize>>,
}

impl Tick for CountingTick {
    fn tick<'a>(&'a mut self, _cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        *self.runs.lock().expect("the counter is not poisoned") += 1;
        Box::pin(async { Ok(()) })
    }
}

/// An `OnReading` body that counts the batches it is handed.
#[derive(Debug, Default)]
pub struct CountingOnReading {
    /// How many batches have been delivered.
    pub batches: Arc<Mutex<usize>>,
    /// How many readings, across all of them.
    pub readings: Arc<Mutex<usize>>,
}

impl mw_component::OnReading for CountingOnReading {
    fn readings<'a>(
        &'a mut self,
        batch: ReadingBatch,
        _cx: &'a Cx,
    ) -> BoxFuture<'a, Result<(), ComponentError>> {
        *self.batches.lock().expect("the counter is not poisoned") += 1;
        *self.readings.lock().expect("the counter is not poisoned") += batch.len();
        Box::pin(async { Ok(()) })
    }
}

/// An `OnReading` body that takes a batch and never returns — the stalled sink.
#[derive(Debug)]
pub struct StalledOnReading;

impl mw_component::OnReading for StalledOnReading {
    fn readings<'a>(
        &'a mut self,
        _batch: ReadingBatch,
        _cx: &'a Cx,
    ) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(std::future::pending())
    }
}

/// A `Tick` body that always fails, with a cause chain to render.
#[derive(Debug, Default)]
pub struct FailingTick {
    /// How many occurrences have failed.
    pub runs: Arc<Mutex<usize>>,
}

/// The cause a [`FailingTick`] reports.
#[derive(Debug, thiserror::Error)]
#[error("connection refused (os error 111)")]
pub struct Refused;

impl Tick for FailingTick {
    fn tick<'a>(&'a mut self, _cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        *self.runs.lock().expect("the counter is not poisoned") += 1;
        Box::pin(async { Err(ComponentError::tick("upload to the test service", Refused)) })
    }
}

/// A `Tick` body that ignores its deadline and its cancellation token.
#[derive(Debug, Default)]
pub struct StalledTick {
    /// How many occurrences have been started.
    pub runs: Arc<Mutex<usize>>,
}

impl Tick for StalledTick {
    fn tick<'a>(&'a mut self, _cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        *self.runs.lock().expect("the counter is not poisoned") += 1;
        Box::pin(std::future::pending())
    }
}

/// A `Serve` body that ignores cancellation and never returns.
///
/// The stalled sink of W3 §8.4's second concurrency test: what it proves is that shutdown
/// completes anyway, inside the grace plus a margin, because the task table aborts it.
#[derive(Debug)]
pub struct StalledServe;

impl Serve for StalledServe {
    fn serve(self: Box<Self>, _cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>> {
        // Parked forever and deaf to the token, which is the point: a body that cooperates
        // proves nothing about the grace deadline.
        Box::pin(std::future::pending())
    }
}

/// A `Serve` body that returns as soon as it is cancelled.
#[derive(Debug)]
pub struct CooperativeServe;

impl Serve for CooperativeServe {
    fn serve(self: Box<Self>, cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>> {
        Box::pin(async move {
            cx.cancelled().await;
            Ok(())
        })
    }
}

/// A spec for a `Tick` component on a period, with a counter the test can read.
pub fn counting_every(
    id: &str,
    period: Duration,
    deadline: Duration,
) -> (ComponentSpec, Arc<Mutex<usize>>) {
    let runs = Arc::new(Mutex::new(0));
    let shared = Arc::clone(&runs);
    let spec = ComponentSpec::new(
        ComponentId::new(id).expect("a test id is well formed"),
        Trigger::every(period, deadline),
        Builder::tick(move |_| {
            let runs = Arc::clone(&shared);
            async move { Ok(CountingTick { runs }) }
        }),
    );
    (spec, runs)
}

/// A spec for a `Service` component whose body is `body`.
pub fn service<F, T>(id: &str, body: F) -> ComponentSpec
where
    F: Fn() -> T + Send + Sync + 'static,
    T: Serve + 'static,
{
    ComponentSpec::new(
        ComponentId::new(id).expect("a test id is well formed"),
        Trigger::Service,
        Builder::serve(move |_| {
            let built = body();
            async move { Ok(built) }
        }),
    )
}

/// An id, for a test that would otherwise unwrap one on every line.
pub fn id(s: &str) -> ComponentId {
    ComponentId::new(s).expect("a test id is well formed")
}
