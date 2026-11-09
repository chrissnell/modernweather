//! [`Cx`] and [`BuildCx`] — what a component is handed, and the typed-handle resolution
//! of W2a §2.3.
//!
//! **Capabilities are resolved once, in `build`, and are not reachable afterwards.** A
//! registry lookup at the point of use (`cx.get::<ReadingStore>()?` inside a tick) means
//! every call site can fail, so "the database is missing" becomes an error branch in
//! business logic instead of a lifecycle state — which is exactly the shape being removed.
//! That is why there are two context types rather than one: [`BuildCx`] can resolve
//! handles and [`Cx`] cannot, so *after* `build` a component cannot observe absence at
//! all. W2a's sketch passes one `&Cx` to both phases; splitting it is how Decision 21
//! becomes structural rather than a convention.
//!
//! The same applies in the other direction: a provider is handed a [`Publisher`] for each
//! capability **it declared**, so publishing something undeclared is not a runtime error
//! to handle but a handle that was never issued.
//!
//! **The claim, stated precisely.** For an *exclusive* capability a built component holds
//! a plain handle and has no way to ask whether it is still published, so unavailability
//! reaches it as a lifecycle state rather than as an error at a call site. For the one
//! *additive* capability the opposite is required — the listener must watch route
//! providers arrive and leave (Decision 19) — so [`BuildCx::additive`] is restricted to
//! that arity and nothing else can be taken as a watch.
//!
//! What is **not** enforced: a body could store its whole [`BuildCx`] and resolve at use
//! time. Closing that would mean the build closure borrowing the context rather than
//! owning it, which is an API shape the next six work items would have to be written
//! against, so it is a reviewable mistake rather than an unrepresentable one — flagged
//! here so the distinction is not read the other way.

use std::any::Any;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::capability::{Arity, Capability, Need, Strength};
use crate::control::Generation;
use crate::error::BuildError;
use crate::id::ComponentId;

/// A capability handle: the type a provider publishes and a consumer resolves.
///
/// Implemented on `Arc<dyn SomeTrait>` rather than on a concrete type, because rule 1 of
/// docs/service-architecture.md §1.3 is that the handle *trait* lives in this crate and
/// its implementation lives in the providing crate — so what travels through the
/// capability bus is a trait object, and the binary is the only place the two meet.
pub trait CapabilityHandle: Clone + Send + Sync + 'static {
    /// Which capability this handle is for. The association is on the type, so a consumer
    /// cannot resolve `ReadingStore` and be handed `SnowCache`.
    const CAPABILITY: Capability;
}

/// One published handle, with the component that published it.
#[derive(Clone)]
pub struct Published {
    /// Who published it. `Waiting` carries this so W5 can render an actionable sentence.
    pub provider: ComponentId,
    /// The handle, type-erased for the bus.
    pub handle: Arc<dyn Any + Send + Sync>,
}

impl fmt::Debug for Published {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Published")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

/// The current providers of one capability.
///
/// Empty means unavailable, which is the state every slot starts in: **readiness is
/// explicit and distinct from `Running`** (W2a Decision 22), because `db:postgres` is
/// `Running` from the moment its task starts and usable only after a probe succeeds.
/// Publication through a watch is also what makes revocation possible, which is what makes
/// `Waiting` re-enterable.
#[derive(Debug, Clone, Default)]
pub struct CapabilitySlot {
    /// Zero or one provider for an exclusive capability; any number for an additive one.
    pub providers: Vec<Published>,
}

impl CapabilitySlot {
    /// Whether anything provides this capability right now.
    pub fn is_available(&self) -> bool {
        !self.providers.is_empty()
    }

    /// The single provider of an exclusive capability.
    pub fn sole(&self) -> Option<&Published> {
        self.providers.first()
    }
}

/// The set of capability watches a component may read, with the needs it declared.
///
/// Constructed by the supervisor and moved into [`BuildCx`].
#[derive(Debug, Clone)]
pub struct CapabilityAccess {
    needs: Arc<[Need]>,
    provides: Arc<[Capability]>,
    slots: Arc<[(Capability, watch::Receiver<Arc<CapabilitySlot>>)]>,
    publisher: Arc<dyn CapabilityPublisher>,
}

impl CapabilityAccess {
    /// Assemble the access a component's declarations entitle it to.
    pub fn new(
        needs: Arc<[Need]>,
        provides: Arc<[Capability]>,
        slots: Vec<(Capability, watch::Receiver<Arc<CapabilitySlot>>)>,
        publisher: Arc<dyn CapabilityPublisher>,
    ) -> Self {
        Self {
            needs,
            provides,
            slots: slots.into(),
            publisher,
        }
    }

    fn slot(&self, capability: Capability) -> Option<&watch::Receiver<Arc<CapabilitySlot>>> {
        self.slots
            .iter()
            .find(|(c, _)| *c == capability)
            .map(|(_, rx)| rx)
    }

    fn declared(&self, capability: Capability) -> Option<&Need> {
        self.needs.iter().find(|n| n.capability == capability)
    }
}

/// How a provider's publication reaches the supervisor.
///
/// A trait rather than a channel type, because the supervisor owns the protocol and this
/// crate must not know it: the supervisor is the only owner of component state, so a
/// publication is a message to its one `select!` loop rather than a lock a component
/// takes.
pub trait CapabilityPublisher: Send + Sync + fmt::Debug {
    /// Publish or replace this provider's handle for `capability`.
    fn publish(
        &self,
        provider: &ComponentId,
        capability: Capability,
        handle: Arc<dyn Any + Send + Sync>,
    );

    /// Withdraw this provider's handle. Consumers with a hard need return to `Waiting`.
    fn revoke(&self, provider: &ComponentId, capability: Capability);
}

/// A provider's typed publication handle for one declared capability.
///
/// Issued by [`BuildCx::publisher`] and only for a capability the component declared, so
/// there is no "publishing something undeclared" error path at run time.
pub struct Publisher<H: CapabilityHandle> {
    provider: ComponentId,
    bus: Arc<dyn CapabilityPublisher>,
    _handle: PhantomData<fn() -> H>,
}

impl<H: CapabilityHandle> fmt::Debug for Publisher<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Publisher")
            .field("provider", &self.provider)
            .field("capability", &H::CAPABILITY)
            .finish()
    }
}

impl<H: CapabilityHandle> Clone for Publisher<H> {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            bus: Arc::clone(&self.bus),
            _handle: PhantomData,
        }
    }
}

impl<H: CapabilityHandle> Publisher<H> {
    /// Announce readiness with this handle. Replaces any handle this provider published
    /// before, which is how a reconnected pool republishes without a revoke first.
    pub fn publish(&self, handle: H) {
        self.bus
            .publish(&self.provider, H::CAPABILITY, Arc::new(handle));
    }

    /// Withdraw the handle, because the capability is genuinely unusable — a dead
    /// connection pool, not a single failed query (W2a §3.4).
    pub fn revoke(&self) {
        self.bus.revoke(&self.provider, H::CAPABILITY);
    }
}

/// Many providers of one additive capability, as a live view.
///
/// The consumer starts with whatever is published and accepts more as providers become
/// ready. **The listener must not wait for all route providers** (W2a Decision 19): a
/// management UI that is unreachable precisely when something is broken is the worst
/// possible failure mode.
pub struct Additive<H: CapabilityHandle> {
    rx: watch::Receiver<Arc<CapabilitySlot>>,
    _handle: PhantomData<fn() -> H>,
}

impl<H: CapabilityHandle> fmt::Debug for Additive<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Additive")
            .field("capability", &H::CAPABILITY)
            .field("providers", &self.rx.borrow().providers.len())
            .finish()
    }
}

impl<H: CapabilityHandle> Clone for Additive<H> {
    fn clone(&self) -> Self {
        Self {
            rx: self.rx.clone(),
            _handle: PhantomData,
        }
    }
}

impl<H: CapabilityHandle> Additive<H> {
    /// Every provider's handle, now.
    ///
    /// A handle published under the wrong type is skipped rather than failing the whole
    /// load: one misconfigured route provider must not take the listener down with it.
    /// It is **logged here**, by the consumer, because only the consumer knows the type it
    /// expected — the supervisor moves the handle as an `Arc<dyn Any>` and never learns
    /// what it should have been. Without that line the affected paths would answer 503
    /// forever with nothing anywhere saying why.
    pub fn load(&self) -> Vec<(ComponentId, H)> {
        self.rx
            .borrow()
            .providers
            .iter()
            .filter_map(|p| match p.handle.downcast_ref::<H>() {
                Some(h) => Some((p.provider.clone(), h.clone())),
                None => {
                    tracing::warn!(
                        provider = %p.provider,
                        capability = %H::CAPABILITY,
                        expected = std::any::type_name::<H>(),
                        "a published handle is not the type this consumer resolves; skipped"
                    );
                    None
                }
            })
            .collect()
    }

    /// Resolves when the provider set changes, so a consumer can rebuild.
    pub async fn changed(&mut self) {
        // An `Err` means every sender is gone, which happens only during shutdown; a
        // consumer that is awaiting a change then has nothing more to rebuild for.
        let _ = self.rx.changed().await;
    }
}

/// A **soft, exclusive** capability's handle, read when it is used rather than when the
/// component was built.
///
/// # The defect this type exists to close
///
/// [`BuildCx::soft`] borrows the slot once and returns what is in it. For a component whose
/// hard needs are satisfied in the first reconcile pass that is almost always `None`, and it
/// stays `None` for the life of the process: `mw_supervisor`'s reconcile does not rebuild a
/// `Running` component when a soft capability later arrives — the `(State::Running, true)`
/// arm refreshes the `degraded` label and returns. M4-1's code review found
/// `snapshot:latest` in exactly that state against `WindowedView`, with every snapshot
/// `Degraded` forever; `snapshot:latest`'s answer was to take the handle from the registrar,
/// which works because `RollupState` is a root resource `main` owns.
///
/// **`api:legacy` has no such answer, and that is the whole reason this type is here.** Its
/// three read stores — `ReadingStore`, `ForecastStore` and `SnowCache` — are published by
/// `db:postgres`, whose pool does not exist until its own task has connected and asserted
/// **D-62** ([`crate::Store`]'s module documentation has why that cannot be a root
/// resource). So the handle genuinely arrives after the build, and a once-only read makes
/// `/span`, `/almanac` and `/forecast` permanently empty on a healthy process.
///
/// # Why this is not the hole [`Additive`] declines to open
///
/// [`BuildCx::additive`] refuses an exclusive capability, on the grounds that a component
/// able to watch its database being revoked would decide for itself what that means — W2a
/// Decision 21's "no error branch in business logic". This type is **restricted to a `Soft`
/// need**, and a `Soft` need is the declaration that the component *does* degrade in a
/// defined way: `docs/component-model.md` §3.2's table says so for this exact component and
/// capability — *"`api:legacy` `/forecast` | failing query | `Running { degraded:
/// [ForecastStore] }`, defined stale/empty response"*. Decision 21 is about `Hard` needs,
/// where absence means `Waiting` and there is nothing to decide. Asking for this against a
/// `Hard` need is [`BuildError::UndeclaredNeed`], the same answer [`BuildCx::soft`] gives
/// for the inverse mistake.
///
/// Recorded as **A-M4-2-1** in `docs/reconciliation.md`'s *Proposed amendments*: it resolves
/// a shape the design left between Decision 21 and §3.2's table rather than contradicting a
/// decision.
pub struct Deferred<H: CapabilityHandle> {
    rx: watch::Receiver<Arc<CapabilitySlot>>,
    _handle: PhantomData<fn() -> H>,
}

impl<H: CapabilityHandle> fmt::Debug for Deferred<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deferred")
            .field("capability", &H::CAPABILITY)
            .field("available", &self.rx.borrow().is_available())
            .finish()
    }
}

impl<H: CapabilityHandle> Clone for Deferred<H> {
    fn clone(&self) -> Self {
        Self {
            rx: self.rx.clone(),
            _handle: PhantomData,
        }
    }
}

impl<H: CapabilityHandle> Deferred<H> {
    /// The handle, if a provider has published one.
    ///
    /// `None` is the degraded state the `Soft` need declared, not an error. A handle
    /// published under a type this consumer does not resolve is **also** `None` and is
    /// logged, for [`Additive::load`]'s reason: the supervisor moves it as an
    /// `Arc<dyn Any>` and never learns what it should have been, so without this line the
    /// affected endpoints would serve the empty response forever with nothing saying why.
    pub fn load(&self) -> Option<H> {
        let slot = self.rx.borrow();
        let published = slot.sole()?;
        match published.handle.downcast_ref::<H>() {
            Some(h) => Some(h.clone()),
            None => {
                tracing::warn!(
                    provider = %published.provider,
                    capability = %H::CAPABILITY,
                    expected = std::any::type_name::<H>(),
                    "a published handle is not the type this consumer resolves; the \
                     capability reads as unavailable and the consumer will degrade"
                );
                None
            }
        }
    }

    /// Whether a provider has published anything at all, for a log field and for
    /// `X-MW-Degraded`. Does not check the handle's type; [`Deferred::load`] does.
    pub fn is_available(&self) -> bool {
        self.rx.borrow().is_available()
    }
}

/// What a component's body is handed: identity, cancellation, and the configuration
/// generation it was built from.
///
/// Deliberately **not** a service locator. There is no capability map here; see the module
/// documentation.
#[derive(Debug, Clone)]
pub struct Cx {
    id: ComponentId,
    token: CancellationToken,
    generation: Generation,
    newest_generation: watch::Receiver<Generation>,
}

impl Cx {
    /// Assemble a context. The supervisor is the only caller outside tests.
    pub fn new(
        id: ComponentId,
        token: CancellationToken,
        generation: Generation,
        newest_generation: watch::Receiver<Generation>,
    ) -> Self {
        Self {
            id,
            token,
            generation,
            newest_generation,
        }
    }

    /// This component's id. Also the span field every log line from this component
    /// carries, because the supervisor instruments the body's future with it.
    pub fn id(&self) -> &ComponentId {
        &self.id
    }

    /// This component's cancellation token.
    ///
    /// There is no stop method anywhere in the abstraction (**D-24**): a component observes
    /// cancellation at its next await point and `Drop` releases what it holds, which is
    /// what makes "removing a controller leaks its listeners" structurally impossible
    /// rather than merely fixed.
    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    /// Resolves when this component is cancelled.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    /// Whether cancellation has already been requested — the check a `Serve` loop makes
    /// before starting another unit of work.
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// The configuration generation this instance was **built from**.
    ///
    /// This is what `ComponentControl` reports as `applied_generation` (**D-53**), and it
    /// is per component on purpose: a global "configuration version applied" would be
    /// wrong the moment one component restarts and another does not, which is the common
    /// case, since unchanged components are deliberately not touched.
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// The newest committed configuration generation.
    ///
    /// W2a Decision 16: the config watch is a **root fact** in every context rather than a
    /// capability everything declares. The *mutating* side is `ConfigAdmin`, which exactly
    /// one component requires, and the supervisor is the only thing that acts on a change —
    /// so what a component needs from the watch is "am I stale", not "re-read the file".
    pub fn newest_generation(&self) -> Generation {
        *self.newest_generation.borrow()
    }

    /// A receiver for the configuration generation, for a body that wants to await a
    /// change rather than poll it.
    pub fn generation_watch(&self) -> watch::Receiver<Generation> {
        self.newest_generation.clone()
    }
}

/// What a component's **builder** is handed: a [`Cx`], plus the one-time capability
/// resolution.
#[derive(Debug, Clone)]
pub struct BuildCx {
    cx: Cx,
    access: CapabilityAccess,
}

impl BuildCx {
    /// Assemble a build context. The supervisor is the only caller outside tests.
    pub fn new(cx: Cx, access: CapabilityAccess) -> Self {
        Self { cx, access }
    }

    /// The context the built body will keep.
    pub fn cx(&self) -> &Cx {
        &self.cx
    }

    /// Take the context, for a body that stores it.
    pub fn into_cx(self) -> Cx {
        self.cx
    }

    /// Resolve a hard requirement.
    ///
    /// Three failures, each with a distinct name: the capability was not declared
    /// (rule 3 — a component is handed only what it declares), it is not available (which
    /// the supervisor does not normally allow, since it builds only once the hard needs
    /// are published), or the provider published a different type.
    pub fn hard<H: CapabilityHandle>(&self) -> Result<H, BuildError> {
        let need = self.require_declared(H::CAPABILITY)?;
        if need.strength != Strength::Hard {
            // Resolving a soft need as hard would reintroduce the thing `Soft` exists to
            // prevent: a component that cannot start because something optional is down.
            return Err(BuildError::UndeclaredNeed {
                capability: H::CAPABILITY,
            });
        }
        self.resolve::<H>()?.ok_or(BuildError::Unavailable {
            capability: H::CAPABILITY,
        })
    }

    /// Resolve a soft requirement.
    ///
    /// `None` is a defined state, not an error: the component starts regardless and
    /// degrades in a way written down in the one component that degrades, rather than
    /// handled everywhere.
    pub fn soft<H: CapabilityHandle>(&self) -> Result<Option<H>, BuildError> {
        self.require_declared(H::CAPABILITY)?;
        self.resolve::<H>()
    }

    /// Subscribe to every provider of an additive capability.
    ///
    /// Only an additive one. An `Additive<H>` is a live `watch::Receiver` the consumer
    /// keeps after `build`, so taking an *exclusive* capability this way would be the one
    /// hole in "after `build` a component cannot observe absence": it could watch its
    /// database being revoked and decide for itself what that means, which is the error
    /// branch in business logic that Decision 21 exists to remove.
    pub fn additive<H: CapabilityHandle>(&self) -> Result<Additive<H>, BuildError> {
        if H::CAPABILITY.arity() != Arity::Additive {
            return Err(BuildError::NotAdditive {
                capability: H::CAPABILITY,
            });
        }
        self.require_declared(H::CAPABILITY)?;
        let rx = self
            .access
            .slot(H::CAPABILITY)
            .ok_or(BuildError::UndeclaredNeed {
                capability: H::CAPABILITY,
            })?
            .clone();
        Ok(Additive {
            rx,
            _handle: PhantomData,
        })
    }

    /// The publication handle for a capability this component **provides**.
    pub fn publisher<H: CapabilityHandle>(&self) -> Result<Publisher<H>, BuildError> {
        if !self.access.provides.contains(&H::CAPABILITY) {
            return Err(BuildError::UndeclaredProvision {
                capability: H::CAPABILITY,
            });
        }
        Ok(Publisher {
            provider: self.cx.id.clone(),
            bus: Arc::clone(&self.access.publisher),
            _handle: PhantomData,
        })
    }

    /// A handle for a **soft** requirement, read at use rather than at build.
    ///
    /// [`Deferred`]'s own documentation has the whole of why this exists and why it is not
    /// the hole [`BuildCx::additive`] declines to open. Two refusals, each with a distinct
    /// name: the capability was not declared (rule 3), or it was declared `Hard` — for
    /// which [`BuildCx::hard`] is the right call and absence means `Waiting` rather than
    /// degradation.
    pub fn deferred<H: CapabilityHandle>(&self) -> Result<Deferred<H>, BuildError> {
        let need = self.require_declared(H::CAPABILITY)?;
        if need.strength != Strength::Soft {
            return Err(BuildError::UndeclaredNeed {
                capability: H::CAPABILITY,
            });
        }
        let rx = self
            .access
            .slot(H::CAPABILITY)
            .ok_or(BuildError::Unavailable {
                capability: H::CAPABILITY,
            })?
            .clone();
        Ok(Deferred {
            rx,
            _handle: PhantomData,
        })
    }

    fn require_declared(&self, capability: Capability) -> Result<&Need, BuildError> {
        self.access
            .declared(capability)
            .ok_or(BuildError::UndeclaredNeed { capability })
    }

    fn resolve<H: CapabilityHandle>(&self) -> Result<Option<H>, BuildError> {
        let Some(rx) = self.access.slot(H::CAPABILITY) else {
            return Ok(None);
        };
        let slot = rx.borrow();
        match slot.sole() {
            None => Ok(None),
            Some(published) => published
                .handle
                .downcast_ref::<H>()
                .cloned()
                .map(Some)
                .ok_or(BuildError::HandleTypeMismatch {
                    capability: H::CAPABILITY,
                    expected: std::any::type_name::<H>(),
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct FakeStore(&'static str);
    impl CapabilityHandle for FakeStore {
        const CAPABILITY: Capability = Capability::ReadingStore;
    }

    #[derive(Clone)]
    struct FakeRoutes;
    impl CapabilityHandle for FakeRoutes {
        const CAPABILITY: Capability = Capability::HttpRoutes;
    }

    #[derive(Debug, Default)]
    struct RecordingBus(Mutex<Vec<(ComponentId, Capability, bool)>>);

    impl CapabilityPublisher for RecordingBus {
        fn publish(
            &self,
            provider: &ComponentId,
            capability: Capability,
            _handle: Arc<dyn Any + Send + Sync>,
        ) {
            self.0
                .lock()
                .unwrap()
                .push((provider.clone(), capability, true));
        }
        fn revoke(&self, provider: &ComponentId, capability: Capability) {
            self.0
                .lock()
                .unwrap()
                .push((provider.clone(), capability, false));
        }
    }

    fn id(s: &str) -> ComponentId {
        ComponentId::new(s).unwrap()
    }

    fn build_cx(
        needs: Vec<Need>,
        provides: Vec<Capability>,
        slots: Vec<(Capability, CapabilitySlot)>,
        bus: Arc<dyn CapabilityPublisher>,
    ) -> BuildCx {
        let (_gen_tx, gen_rx) = watch::channel(Generation::ZERO);
        let cx = Cx::new(
            id("writer:timescale"),
            CancellationToken::new(),
            Generation::ZERO,
            gen_rx,
        );
        let slots = slots
            .into_iter()
            .map(|(c, slot)| {
                let (tx, rx) = watch::channel(Arc::new(slot));
                // The supervisor holds every sender for the life of the process; a test
                // that dropped them here would still read `borrow()` but would see
                // `changed()` end immediately.
                std::mem::forget(tx);
                (c, rx)
            })
            .collect();
        BuildCx::new(
            cx,
            CapabilityAccess::new(needs.into(), provides.into(), slots, bus),
        )
    }

    fn slot_with(provider: &str, handle: Arc<dyn Any + Send + Sync>) -> CapabilitySlot {
        CapabilitySlot {
            providers: vec![Published {
                provider: id(provider),
                handle,
            }],
        }
    }

    #[test]
    fn a_declared_hard_need_resolves_to_its_typed_handle() {
        let cx = build_cx(
            vec![Need::hard(Capability::ReadingStore)],
            vec![],
            vec![(
                Capability::ReadingStore,
                slot_with("db:postgres", Arc::new(FakeStore("pool"))),
            )],
            Arc::new(RecordingBus::default()),
        );
        assert_eq!(cx.hard::<FakeStore>().unwrap().0, "pool");
    }

    /// Rule 3 of docs/service-architecture.md §1.3, as a test: the thing that must not
    /// query cannot, because it was never handed a handle that can.
    #[test]
    fn an_undeclared_need_cannot_be_resolved_even_when_it_is_published() {
        let cx = build_cx(
            vec![Need::hard(Capability::LatestSnapshot)],
            vec![],
            vec![(
                Capability::ReadingStore,
                slot_with("db:postgres", Arc::new(FakeStore("pool"))),
            )],
            Arc::new(RecordingBus::default()),
        );
        assert!(matches!(
            cx.hard::<FakeStore>(),
            Err(BuildError::UndeclaredNeed {
                capability: Capability::ReadingStore
            })
        ));
    }

    #[test]
    fn a_soft_need_resolves_to_none_rather_than_an_error() {
        let cx = build_cx(
            vec![Need::soft(Capability::ReadingStore)],
            vec![],
            vec![(Capability::ReadingStore, CapabilitySlot::default())],
            Arc::new(RecordingBus::default()),
        );
        assert!(cx.soft::<FakeStore>().unwrap().is_none());
        // ...and the same need cannot be resolved as hard, which would make an optional
        // capability able to hold a component down.
        assert!(matches!(
            cx.hard::<FakeStore>(),
            Err(BuildError::UndeclaredNeed { .. })
        ));
    }

    #[test]
    fn a_handle_of_the_wrong_type_is_named_rather_than_panicking() {
        let cx = build_cx(
            vec![Need::hard(Capability::ReadingStore)],
            vec![],
            vec![(
                Capability::ReadingStore,
                slot_with("db:postgres", Arc::new(FakeRoutes)),
            )],
            Arc::new(RecordingBus::default()),
        );
        assert!(matches!(
            cx.hard::<FakeStore>(),
            Err(BuildError::HandleTypeMismatch { .. })
        ));
    }

    #[test]
    fn only_a_declared_provision_yields_a_publisher() {
        let bus = Arc::new(RecordingBus::default());
        let cx = build_cx(
            vec![],
            vec![Capability::HttpRoutes],
            vec![],
            bus.clone() as Arc<dyn CapabilityPublisher>,
        );
        let publisher = cx.publisher::<FakeRoutes>().unwrap();
        publisher.publish(FakeRoutes);
        publisher.revoke();
        assert!(matches!(
            cx.publisher::<FakeStore>(),
            Err(BuildError::UndeclaredProvision {
                capability: Capability::ReadingStore
            })
        ));
        let seen = bus.0.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                (id("writer:timescale"), Capability::HttpRoutes, true),
                (id("writer:timescale"), Capability::HttpRoutes, false),
            ]
        );
    }

    /// An exclusive capability taken as a live subscription would be a component watching
    /// its own database being revoked — the error branch in business logic that resolving
    /// once exists to remove.
    #[test]
    fn an_exclusive_capability_cannot_be_taken_as_an_additive_subscription() {
        let cx = build_cx(
            vec![Need::hard(Capability::ReadingStore)],
            vec![],
            vec![(
                Capability::ReadingStore,
                slot_with("db:postgres", Arc::new(FakeStore("pool"))),
            )],
            Arc::new(RecordingBus::default()),
        );
        assert!(matches!(
            cx.additive::<FakeStore>(),
            Err(BuildError::NotAdditive {
                capability: Capability::ReadingStore
            })
        ));
    }

    #[test]
    fn an_additive_capability_loads_every_provider() {
        let slot = CapabilitySlot {
            providers: vec![
                Published {
                    provider: id("api:legacy"),
                    handle: Arc::new(FakeRoutes),
                },
                Published {
                    provider: id("api:management"),
                    handle: Arc::new(FakeRoutes),
                },
                // A provider that published the wrong type is skipped, not fatal.
                Published {
                    provider: id("web:site:1"),
                    handle: Arc::new(FakeStore("not a route table")),
                },
            ],
        };
        let cx = build_cx(
            vec![Need::hard(Capability::HttpRoutes)],
            vec![],
            vec![(Capability::HttpRoutes, slot)],
            Arc::new(RecordingBus::default()),
        );
        let routes = cx.additive::<FakeRoutes>().unwrap();
        let owners: Vec<_> = routes.load().into_iter().map(|(id, _)| id).collect();
        assert_eq!(owners, vec![id("api:legacy"), id("api:management")]);
    }
}
