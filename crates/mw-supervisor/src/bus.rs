//! The capability watches: one per capability, published through by providers and read by
//! `build`.
//!
//! Each slot starts at *unavailable* and moves to *ready* when a provider signals
//! readiness, which is why **readiness is explicit and distinct from `Running`** (W2a
//! Decision 22): `db:postgres` is `Running` from the moment its task starts and usable
//! only after a probe succeeds. Publication through a watch is also what makes revocation
//! possible, which is what makes `Waiting` re-enterable — the one case the Go version
//! cannot survive is a dependency that goes away and comes back.
//!
//! A publication is a **message to the supervisor's loop**, not a lock a provider takes.
//! That is what keeps "one owner of component state, no locks on component state at all"
//! true (W3 §3.2).

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use mw_component::{Capability, CapabilityPublisher, CapabilitySlot, ComponentId, Published};
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::task::Event;

/// The component id the runtime spine publishes its root capabilities under.
///
/// A root is not a component: it has no lifecycle, no spec and nothing to wait on. It
/// still needs a name, because `Waiting { on }` carries the provider and W5 renders it.
pub const SPINE: &str = "spine";

/// One watch per capability.
#[derive(Debug)]
pub struct Bus {
    slots: BTreeMap<Capability, Slot>,
    publisher: Arc<EventPublisher>,
}

#[derive(Debug)]
struct Slot {
    tx: watch::Sender<Arc<CapabilitySlot>>,
    rx: watch::Receiver<Arc<CapabilitySlot>>,
}

impl Bus {
    /// A bus with every capability's slot created and empty.
    ///
    /// All twelve exist from the start rather than being created on first publication, so
    /// a consumer that builds before its provider has ever published gets an empty slot
    /// instead of a missing one — the difference between "not ready yet" and "not in this
    /// build at all", which `plan()` has already ruled out.
    pub fn new(events: mpsc::UnboundedSender<Event>) -> Self {
        let mut slots = BTreeMap::new();
        for capability in Capability::ALL {
            let (tx, rx) = watch::channel(Arc::new(CapabilitySlot::default()));
            slots.insert(*capability, Slot { tx, rx });
        }
        Self {
            slots,
            publisher: Arc::new(EventPublisher { events }),
        }
    }

    /// The publisher handle a component's `BuildCx` is given.
    pub fn publisher(&self) -> Arc<dyn CapabilityPublisher> {
        Arc::clone(&self.publisher) as Arc<dyn CapabilityPublisher>
    }

    /// A receiver for one capability's slot.
    pub fn receiver(&self, capability: Capability) -> watch::Receiver<Arc<CapabilitySlot>> {
        self.slots
            .get(&capability)
            .map(|slot| slot.rx.clone())
            .expect("every capability has a slot")
    }

    /// Whether anything provides a capability right now.
    pub fn is_available(&self, capability: Capability) -> bool {
        self.slots
            .get(&capability)
            .is_some_and(|slot| slot.rx.borrow().is_available())
    }

    /// Record a publication. Replaces this provider's previous handle, which is how a
    /// reconnected pool republishes without revoking first.
    ///
    /// Returns whether the capability's availability changed, because that is what decides
    /// whether a consumer can leave `Waiting`.
    pub fn publish(
        &mut self,
        provider: ComponentId,
        capability: Capability,
        handle: Arc<dyn Any + Send + Sync>,
    ) -> bool {
        let Some(slot) = self.slots.get_mut(&capability) else {
            return false;
        };
        let before = slot.rx.borrow().is_available();
        let mut next = (*slot.rx.borrow().clone()).clone();
        next.providers.retain(|p| p.provider != provider);
        next.providers.push(Published { provider, handle });
        // One provider for an exclusive capability is a plan-time guarantee, so ordering
        // only matters for the additive one, where it is plan order by construction.
        next.providers.sort_by(|a, b| a.provider.cmp(&b.provider));
        slot.tx.send_replace(Arc::new(next));
        !before
    }

    /// Withdraw a provider's handle.
    ///
    /// Returns whether the capability became unavailable, which is what moves every
    /// consumer with a hard need back to `Waiting`. Only the provider revokes, and only
    /// when the capability is genuinely unusable — a dead connection pool, not a single
    /// failed query (W2a §3.4).
    pub fn revoke(&mut self, provider: &ComponentId, capability: Capability) -> bool {
        let Some(slot) = self.slots.get_mut(&capability) else {
            return false;
        };
        let before = slot.rx.borrow().is_available();
        let mut next = (*slot.rx.borrow().clone()).clone();
        next.providers.retain(|p| p.provider != *provider);
        let after = next.is_available();
        slot.tx.send_replace(Arc::new(next));
        before && !after
    }

    /// Withdraw everything one provider published — what stopping a component does.
    pub fn revoke_all(&mut self, provider: &ComponentId) -> Vec<Capability> {
        let capabilities: Vec<Capability> = self.slots.keys().copied().collect();
        capabilities
            .into_iter()
            .filter(|c| self.revoke(provider, *c))
            .collect()
    }
}

/// Turns a provider's publication into an event on the supervisor's one loop.
#[derive(Debug)]
struct EventPublisher {
    events: mpsc::UnboundedSender<Event>,
}

impl CapabilityPublisher for EventPublisher {
    fn publish(
        &self,
        provider: &ComponentId,
        capability: Capability,
        handle: Arc<dyn Any + Send + Sync>,
    ) {
        // Unbounded, and that is the decision rather than laziness: a bounded channel
        // would make a provider's announcement either block it (a component waiting on the
        // supervisor, which is backwards) or be dropped (a consumer left `Waiting` for a
        // capability that is in fact ready, with nothing to retry it). Publications are
        // rare -- one per provider per readiness change -- so the queue has no growth
        // story to speak of.
        if self
            .events
            .send(Event::Publish {
                provider: provider.clone(),
                capability,
                handle,
            })
            .is_err()
        {
            debug!(%capability, "the supervisor is gone; publication dropped");
        }
    }

    fn revoke(&self, provider: &ComponentId, capability: Capability) {
        if self
            .events
            .send(Event::Revoke {
                provider: provider.clone(),
                capability,
            })
            .is_err()
        {
            debug!(%capability, "the supervisor is gone; revocation dropped");
        }
    }
}
