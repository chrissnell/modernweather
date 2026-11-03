//! What the reading stream carries (docs/service-architecture.md §2.2).
//!
//! **Scope.** This file exists because the component model needs a type for the thing a
//! `Trigger::OnReading` component is handed (W2a §5), and that type is an envelope rather
//! than a reading. What it deliberately does **not** contain is the *construction* path:
//! `Ingress::admit`, `Envelope::forward` and the loop-detection result belong to
//! implementation-plan.md M3-2, which owns the spool, the fan-out and D-60. So
//! [`Provenance`] has exactly one constructor here — the local-admission one the
//! supervisor's own tests need — and forwarding arrives with the hop that does it.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use smallvec::SmallVec;

use crate::id::{OriginId, Seq, StationKey};
use crate::Reading;

/// One reading, with the identity resolution and provenance that make it storable.
///
/// `reading` is an `Arc` because a batch is fanned out to several subscribers and the
/// struct is large: 212 `Option<f32>` fields plus four dimensions.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    /// The resolved station. There is no envelope without one (W2a §4a.6).
    pub station: StationKey,
    /// When the station says it measured.
    pub observed_at: DateTime<Utc>,
    /// When this node admitted it.
    pub received_at: DateTime<Utc>,
    /// Origin, path and sequence.
    pub provenance: Provenance,
    /// The reading itself.
    pub reading: Arc<Reading>,
}

/// Origin, path and sequence — W7's multi-hop loop prevention.
///
/// The fields are private and there is no `set_origin`, no `set_path` and no `Default`:
/// the origin of a reading is a fact established once, at admission, and a type that lets
/// a later hop rewrite it is a type that cannot prevent a loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    origin: OriginId,
    path: PathSet,
    seq: Seq,
}

impl Provenance {
    /// The provenance of a reading admitted **locally**, by this node.
    ///
    /// The path is the origin alone, because a locally admitted reading has traversed one
    /// node. The forwarding constructor is M3-2's, together with the ack protocol that
    /// makes it meaningful.
    pub fn admitted(origin: OriginId, seq: Seq) -> Self {
        let mut path = PathSet::default();
        path.push(origin);
        Self { origin, path, seq }
    }

    /// The node that first admitted this reading. Never rewritten.
    pub fn origin(&self) -> OriginId {
        self.origin
    }

    /// The per-`(origin, station)` sequence number.
    pub fn seq(&self) -> Seq {
        self.seq
    }

    /// Every node this envelope has traversed, in order, including the origin.
    pub fn path(&self) -> &PathSet {
        &self.path
    }

    /// Whether `node` has already seen this envelope — the loop test.
    pub fn has_visited(&self, node: OriginId) -> bool {
        self.path.contains(node)
    }
}

/// The ordered set of nodes an envelope has traversed (**D-59**).
///
/// `SmallVec<[OriginId; 8]>` rather than a `HashSet`: it is ordered, because "which way
/// did this reading come" is the question an operator asks, and eight hops is far beyond
/// any topology this system is for — so the common case is one inline allocation of none.
///
/// Eight is the inline capacity, **not** a limit: a ninth hop spills to the heap rather
/// than being refused. Whether a path length is rejected belongs with forwarding, which is
/// M3-2's, together with the `Envelope::forward` that is the only thing able to grow one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathSet(SmallVec<[OriginId; 8]>);

impl PathSet {
    /// Append a node. Duplicates are not inserted: the path is a set with an order.
    pub fn push(&mut self, node: OriginId) {
        if !self.contains(node) {
            self.0.push(node);
        }
    }

    /// Whether `node` appears in the path.
    pub fn contains(&self, node: OriginId) -> bool {
        self.0.contains(&node)
    }

    /// The hops, in traversal order.
    pub fn hops(&self) -> &[OriginId] {
        &self.0
    }

    /// How many nodes this envelope has traversed.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the path is empty, which only an unadmitted envelope could be.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(n: u8) -> OriginId {
        OriginId::new([n; 16])
    }

    #[test]
    fn a_locally_admitted_envelope_has_visited_its_own_origin() {
        let p = Provenance::admitted(origin(1), Seq::FIRST);
        assert!(p.has_visited(origin(1)));
        assert!(!p.has_visited(origin(2)));
        assert_eq!(p.path().hops(), &[origin(1)]);
    }

    #[test]
    fn the_path_is_a_set_with_an_order() {
        let mut path = PathSet::default();
        path.push(origin(3));
        path.push(origin(1));
        path.push(origin(3));
        assert_eq!(path.hops(), &[origin(3), origin(1)]);
        assert_eq!(path.len(), 2);
    }
}
