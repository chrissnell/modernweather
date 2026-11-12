//! The three executable traits, and the [`Builder`] that produces one.
//!
//! **Why three and not one.** W2a §1.2 asks whether a single `tick` is the right shape for
//! all six triggers and answers no, because there are exactly three distinct contracts and
//! the difference between them is *what the supervisor must supply and when it may expect
//! a return*. A single `tick` would have to mean both "return promptly, I am timing you"
//! and "never return, I am not timing you"; every piece of supervisor machinery then
//! becomes ambiguous, and a supervisor that awaits `tick` before scheduling the next
//! occurrence deadlocks the moment it is handed a listener.
//!
//! **Why `BoxFuture` rather than `#[async_trait]`.** All three traits are used as
//! `Box<dyn …>`, so they must be dyn-compatible, which `async fn` in a trait is not. The
//! usual answer is the `async-trait` crate, and it is deliberately not used: it is not in
//! **D-32**'s table, adding it would be an amendment rather than a manifest edit, and what
//! it would buy is sugar for the three `Box::pin(async move { … })` call sites a component
//! writes. The desugared form is what `async-trait` generates anyway.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use mw_types::{Envelope, Seq};

use crate::cx::{BuildCx, Cx};
use crate::error::{BuildError, ComponentError};
use crate::trigger::BodyKind;

/// A boxed, `Send` future — what a dyn-compatible async method returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Bounded work, once per occurrence.
///
/// MUST return before the declared deadline. A tick that exceeds it is cancelled through a
/// token that is a **child** of the component's, so the occurrence dies and the component
/// does not (docs/service-architecture.md §3.4).
pub trait Tick: Send {
    /// Run one occurrence.
    fn tick<'a>(&'a mut self, cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>>;
}

/// Bounded work over a batch the supervisor supplies.
pub trait OnReading: Send {
    /// Handle one batch. Returning `Ok` is what acknowledges it: for
    /// [`crate::ReadingPolicy::Durable`] the supervisor advances the cursor only then, so
    /// a failed delivery replays the same batch rather than losing it.
    fn readings<'a>(
        &'a mut self,
        batch: ReadingBatch,
        cx: &'a Cx,
    ) -> BoxFuture<'a, Result<(), ComponentError>>;
}

/// Unbounded. Owns its own accept or receive loop.
///
/// Returns only on cancellation or a fatal error. `self: Box<Self>` because a service
/// consumes itself: there is no second call, and a trait that took `&mut self` would imply
/// the supervisor might call it again.
pub trait Serve: Send + 'static {
    /// Run until cancelled.
    fn serve(self: Box<Self>, cx: Cx) -> BoxFuture<'static, Result<(), ComponentError>>;
}

/// A batch of envelopes, as delivered by the supervisor.
///
/// A batch rather than a single reading, because the storage writer's throughput ceiling
/// in the Go version is one `INSERT` per reading behind one blocking channel send — and a
/// batch of one is the degenerate case of a batch, so it costs nothing to express here
/// (W2a Decision 5).
#[derive(Debug, Clone)]
pub struct ReadingBatch {
    envelopes: Arc<[Envelope]>,
    through: Option<Seq>,
}

impl ReadingBatch {
    /// Wrap envelopes for delivery. Called by the fan-out, which is M3-2's.
    pub fn new(envelopes: impl Into<Arc<[Envelope]>>) -> Self {
        Self {
            envelopes: envelopes.into(),
            through: None,
        }
    }

    /// [`ReadingBatch::new`] carrying the **local** spool position the read covered.
    ///
    /// Only a `Durable` subscription has one, which is why the accessor is an `Option`
    /// rather than this being a field on every batch.
    pub fn at(envelopes: impl Into<Arc<[Envelope]>>, through: Seq) -> Self {
        Self {
            envelopes: envelopes.into(),
            through: Some(through),
        }
    }

    /// The highest **local** spool position this batch covered, for a `Durable`
    /// subscription.
    ///
    /// **This is the sender's coordinate space, which is the only one an ack can be
    /// cumulative in** (W7 **D-22**): `IngestBatch.through` is the sender's spool `seq` of
    /// the last envelope in the batch, and for a relay that is a different number from any
    /// envelope's own `(origin, seq)`, because a relay's log interleaves origins. An uplink
    /// cannot derive it from the envelopes it was handed.
    ///
    /// **Not `envelopes.last()`'s position.** A filter applied by the subscription drops
    /// envelopes that still have to advance the cursor (W7 **D-9**), so the covered
    /// position can be past the last surviving envelope — and a batch that was filtered
    /// empty still has one.
    ///
    /// `None` for `Latest` and `Lossy`, which have no cursor and therefore no position: an
    /// uplink declares `Durable`, so a `None` there is a wiring mistake rather than a state
    /// to handle.
    pub fn through(&self) -> Option<Seq> {
        self.through
    }

    /// The envelopes, oldest first.
    pub fn envelopes(&self) -> &[Envelope] {
        &self.envelopes
    }

    /// How many readings are in the batch.
    pub fn len(&self) -> usize {
        self.envelopes.len()
    }

    /// Whether the batch is empty. The supervisor does not deliver one, but a filter
    /// applied downstream can produce one.
    pub fn is_empty(&self) -> bool {
        self.envelopes.is_empty()
    }

    /// The newest envelope — what a `Latest` subscriber actually wants.
    pub fn newest(&self) -> Option<&Envelope> {
        self.envelopes.last()
    }
}

/// Phase 2: the thing that runs *per start attempt*, after the component's hard
/// requirements are available.
///
/// A builder may fail — a serial port can be missing, a certificate can be unreadable —
/// and a build failure is a lifecycle transition rather than a startup abort (W2a
/// Decision 3). That single change is what replaces "four components refuse to construct
/// and stay dead until the process is restarted".
///
/// **`Fn`, not `FnOnce`.** W2a §1.1's sketch writes `Box<dyn FnOnce(...)>`, and that
/// cannot be right: the sentence immediately above it says a build failure leads to
/// `Backoff → Starting`, and a second `Starting` needs a second build. A builder that
/// could run once would make the lifecycle's own retry path unexpressible, so it runs per
/// start attempt and the closure is `Fn` behind an `Arc`.
#[derive(Clone)]
pub enum Builder {
    /// Produces a [`Tick`], for `Every`, `Calendar`, `OnStartup` and `OnDemand`.
    Tick(BuildFn<Box<dyn Tick>>),
    /// Produces an [`OnReading`].
    OnReading(BuildFn<Box<dyn OnReading>>),
    /// Produces a [`Serve`].
    Serve(BuildFn<Box<dyn Serve>>),
}

/// The builder closure: configuration in, a body out, once per start attempt.
pub type BuildFn<T> =
    Arc<dyn Fn(BuildCx) -> BoxFuture<'static, Result<T, BuildError>> + Send + Sync>;

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A closure has nothing to print, and the workspace lints require `Debug` on every
        // public type. What is useful here is which of the three it is.
        f.write_str(match self {
            Builder::Tick(_) => "Builder::Tick",
            Builder::OnReading(_) => "Builder::OnReading",
            Builder::Serve(_) => "Builder::Serve",
        })
    }
}

impl Builder {
    /// Which executable trait this builder produces. Checked against the trigger in
    /// `plan()`, so the mapping in W2a §1.2 is enforced rather than documented.
    pub fn kind(&self) -> BodyKind {
        match self {
            Builder::Tick(_) => BodyKind::Tick,
            Builder::OnReading(_) => BodyKind::OnReading,
            Builder::Serve(_) => BodyKind::Serve,
        }
    }

    /// A builder for a [`Tick`] body.
    pub fn tick<F, Fut, T>(f: F) -> Self
    where
        F: Fn(BuildCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, BuildError>> + Send + 'static,
        T: Tick + 'static,
    {
        Builder::Tick(Arc::new(move |cx| {
            // `f(cx)` is called here rather than inside the async block, so the closure
            // borrows `f` instead of moving it -- which is what lets it be `Fn` and run
            // again on the next start attempt.
            let building = f(cx);
            Box::pin(async move { building.await.map(|b| Box::new(b) as Box<dyn Tick>) })
        }))
    }

    /// A builder for an [`OnReading`] body.
    pub fn on_reading<F, Fut, T>(f: F) -> Self
    where
        F: Fn(BuildCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, BuildError>> + Send + 'static,
        T: OnReading + 'static,
    {
        Builder::OnReading(Arc::new(move |cx| {
            // `f(cx)` is called here rather than inside the async block, so the closure
            // borrows `f` instead of moving it -- which is what lets it be `Fn` and run
            // again on the next start attempt.
            let building = f(cx);
            Box::pin(async move { building.await.map(|b| Box::new(b) as Box<dyn OnReading>) })
        }))
    }

    /// A builder for a [`Serve`] body.
    pub fn serve<F, Fut, T>(f: F) -> Self
    where
        F: Fn(BuildCx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<T, BuildError>> + Send + 'static,
        T: Serve + 'static,
    {
        Builder::Serve(Arc::new(move |cx| {
            // `f(cx)` is called here rather than inside the async block, so the closure
            // borrows `f` instead of moving it -- which is what lets it be `Fn` and run
            // again on the next start attempt.
            let building = f(cx);
            Box::pin(async move { building.await.map(|b| Box::new(b) as Box<dyn Serve>) })
        }))
    }
}
