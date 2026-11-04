//! What the reading stream carries (docs/service-architecture.md §2.2).
//!
//! **Scope.** This file exists because the component model needs a type for the thing a
//! `Trigger::OnReading` component is handed (W2a §5), and that type is an envelope rather
//! than a reading. `Ingress::admit` itself is `mw-pipeline`'s; what lives here is every
//! way a `Provenance` can come into existence, because that set is what makes a relay
//! cycle unrepresentable rather than merely filtered:
//!
//! - [`Provenance::admitted`] — this node measured it. `path == [origin]`.
//! - [`Provenance::sealed`] — it was decoded from a spool record or a peer's wire message,
//!   with the provenance the origin gave it. **D-60**: never re-sealed, never renumbered.
//! - [`Envelope::forward`] — one hop appended, refused when that hop is already on the
//!   path or when the path is full.
//!
//! There is no fourth, and none of the three can rewrite `origin` or `seq`.

use std::fmt;
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
    /// node.
    pub fn admitted(origin: OriginId, seq: Seq) -> Self {
        let mut path = PathSet::default();
        path.push(origin);
        Self { origin, path, seq }
    }

    /// The provenance an envelope **arrives with** — decoded from a spool record or from a
    /// peer's wire message.
    ///
    /// **D-60.** This is the constructor that makes `Ingress::accept` expressible without
    /// re-sealing: `admit` mints `seq` from the local spool's append position, and running
    /// that on a forwarded envelope renumbers it in the receiver's coordinate space and
    /// destroys the `(origin, seq)` identity the cumulative ack and the dedup gate both
    /// depend on. So a received envelope is reconstructed with the provenance it was given
    /// and nothing else.
    ///
    /// The three structural path properties are checked here rather than at the call site,
    /// because this is the only way such a `Provenance` can exist: `path[0] == origin`
    /// (W7 D-6), at most [`PathSet::MAX_HOPS`] elements (W7 D-8), and no element twice.
    /// The three *positional* clauses of W7 D-7's receive check — addressed to me, I am
    /// not an interior hop, `path[len - 2]` is the sending peer — need to know who "me"
    /// and "the peer" are, so they belong to the receiver, not to the type.
    pub fn sealed(origin: OriginId, seq: Seq, hops: &[OriginId]) -> Result<Self, PathProblem> {
        let path = PathSet::try_from_hops(hops)?;
        if path.hops().first() != Some(&origin) {
            return Err(PathProblem::OriginNotFirst { origin });
        }
        Ok(Self { origin, path, seq })
    }

    /// The node that first admitted this reading. Never rewritten.
    pub fn origin(&self) -> OriginId {
        self.origin
    }

    /// The origin node's spool append position for this envelope (W7 **D-4**).
    ///
    /// Per origin **node**, not per `(origin, station)`: `(origin, seq)` is one envelope's
    /// globally unique immutable identity, and a cumulative ack needs one sequence per log.
    /// docs/service-architecture.md §2.2 annotates it the other way; W7 §1.5 narrows it,
    /// and the narrowing is what makes a resume offset a `u64` rather than a vector keyed
    /// by every station an origin has ever had.
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
/// Eight is the inline capacity **and** the limit (W7 **D-8**, [`PathSet::MAX_HOPS`]): the
/// two coincide so that a legal path is never a heap allocation and a ninth hop is a
/// refusal rather than a growth an untrusted peer controls. Both ways a path can grow —
/// [`Envelope::forward`] and [`Provenance::sealed`] — enforce it, and they are the only
/// two.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathSet(SmallVec<[OriginId; 8]>);

impl PathSet {
    /// The most hops an envelope may carry (W7 **D-8**'s `MAX_PATH`).
    ///
    /// Eight is three more than any topology this system has or plausibly grows into, and
    /// the bound exists because an uncapped path is an unbounded allocation driven by a
    /// peer — and, on the receiving side, unbounded growth of the origin intern table.
    pub const MAX_HOPS: usize = 8;

    /// Append a node. Duplicates are not inserted: the path is a set with an order.
    ///
    /// Private, because a `push` that silently ignores a duplicate is right for the
    /// one-element path [`Provenance::admitted`] builds and wrong for everything else:
    /// a hop that is already present is the loop [`Envelope::forward`] must refuse, and a
    /// duplicate in a decoded path is the forgery [`Provenance::sealed`] must reject.
    fn push(&mut self, node: OriginId) {
        if !self.contains(node) {
            self.0.push(node);
        }
    }

    /// The path a decoded envelope claims, checked for the two properties that do not
    /// depend on who is reading it.
    ///
    /// Non-empty and at most [`PathSet::MAX_HOPS`] long, with no element twice. `path[0]
    /// == origin` is [`Provenance::sealed`]'s, because only it knows the origin.
    pub fn try_from_hops(hops: &[OriginId]) -> Result<Self, PathProblem> {
        if hops.is_empty() {
            return Err(PathProblem::Empty);
        }
        if hops.len() > Self::MAX_HOPS {
            return Err(PathProblem::TooLong { len: hops.len() });
        }
        let mut path = Self::default();
        for &hop in hops {
            if path.contains(hop) {
                return Err(PathProblem::Duplicate { node: hop });
            }
            path.push(hop);
        }
        Ok(path)
    }

    /// The hop this envelope is addressed to — `path.last()` (W7 **D-6**).
    ///
    /// For a locally admitted envelope that is the origin itself, which is why
    /// `ReadingFilter::NotFrom(peer)` rather than `NotFrom(self)` is the uplink's filter
    /// (**D-47**).
    pub fn last(&self) -> Option<OriginId> {
        self.0.last().copied()
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

/// Why a decoded path could not be a [`PathSet`].
///
/// Not a `thiserror` enum: §4.1 puts error enums in the nine crates above this one, and
/// the variants here are the input half of a receive check whose counters and
/// `Reject` mapping live with the receiver (`mw-pipeline`). What this crate owes is that
/// the three cases are distinguishable, because "too long" is a bound an operator can
/// raise and "duplicate" is a peer sending something it could not have produced honestly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathProblem {
    /// No hops at all. The origin is always on its own path, so this is not a path any
    /// honest encoder produces.
    Empty,
    /// More than [`PathSet::MAX_HOPS`] hops (W7 `Reject::PathTooLong`).
    TooLong {
        /// How many were claimed.
        len: usize,
    },
    /// The same node twice (W7 `Reject::MalformedPath`).
    Duplicate {
        /// The node that appeared twice.
        node: OriginId,
    },
    /// `path[0]` is not the origin (W7 **D-6**, `Reject::MalformedPath`).
    OriginNotFirst {
        /// The origin the envelope claims.
        origin: OriginId,
    },
}

impl fmt::Display for PathProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathProblem::Empty => f.write_str("the provenance path is empty"),
            PathProblem::TooLong { len } => write!(
                f,
                "the provenance path carries {len} hops, more than the {} allowed",
                PathSet::MAX_HOPS
            ),
            PathProblem::Duplicate { node } => {
                write!(f, "the provenance path names {node} twice")
            }
            PathProblem::OriginNotFirst { origin } => {
                write!(
                    f,
                    "the provenance path does not start at its origin {origin}"
                )
            }
        }
    }
}

impl std::error::Error for PathProblem {}

/// Why an envelope could not be addressed to one more hop.
///
/// Two variants rather than W3 §2.2's single `LoopDetected`, because the two mean opposite
/// things to an operator and W7 **D-41** turns one of them into an assertion:
/// `uplink_loop_refused_total` moving means the uplink's `NotFrom(peer)` filter missed
/// something the invariant then caught — a bug in that code. A full path is not a bug; it
/// is a topology deeper than [`PathSet::MAX_HOPS`].
///
/// The path is boxed. An eight-element inline `SmallVec` is 136 bytes, which would make the
/// `Err` half of [`Envelope::forward`]'s result the expensive half of every *successful*
/// forward — `clippy::result_large_err` says so. The path is carried at all because "which
/// way did this reading come" is the question an operator asks when a loop is refused, and
/// `forward` consumed the envelope it would otherwise be read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardRefused {
    /// `hop` is already on the path. This is W3 §2.2's `LoopDetected`.
    Loop {
        /// The hop that was asked for.
        node: OriginId,
        /// The path it is already on.
        path: Box<PathSet>,
    },
    /// The path already holds [`PathSet::MAX_HOPS`] hops, so a ninth would be refused by
    /// the receiver anyway (W7 **D-8**).
    PathFull {
        /// The path as it stands.
        path: Box<PathSet>,
    },
}

impl fmt::Display for ForwardRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForwardRefused::Loop { node, path } => {
                write!(f, "{node} is already on the path ")?;
                write_hops(f, path)
            }
            ForwardRefused::PathFull { path } => {
                write!(
                    f,
                    "the path already holds the {} hops allowed: ",
                    PathSet::MAX_HOPS
                )?;
                write_hops(f, path)
            }
        }
    }
}

impl std::error::Error for ForwardRefused {}

fn write_hops(f: &mut fmt::Formatter<'_>, path: &PathSet) -> fmt::Result {
    for (i, hop) in path.hops().iter().enumerate() {
        if i > 0 {
            f.write_str("->")?;
        }
        write!(f, "{hop}")?;
    }
    Ok(())
}

impl Envelope {
    /// Address this envelope at one more hop — the **only** way to produce an envelope for
    /// forwarding to a peer (docs/service-architecture.md §2.2).
    ///
    /// Consumes `self`, so the un-forwarded envelope is not still lying around to send as
    /// well, and refuses rather than truncating: an A→B→C→A cycle is not *detected* here,
    /// it is **unrepresentable**, because the value that would have to exist cannot be
    /// built. That is the second of W7 **D-7**'s two independent guards and the one that is
    /// a property of the type rather than of a code path.
    ///
    /// The uplink's `ReadingFilter::NotFrom(peer)` is an optimisation in front of this, not
    /// a substitute for it (W7 §1.4): removing the filter would cost throughput, and
    /// removing this check is not possible without editing this crate.
    pub fn forward(mut self, hop: OriginId) -> Result<Self, ForwardRefused> {
        if self.provenance.path.contains(hop) {
            return Err(ForwardRefused::Loop {
                node: hop,
                path: Box::new(self.provenance.path.clone()),
            });
        }
        if self.provenance.path.len() >= PathSet::MAX_HOPS {
            return Err(ForwardRefused::PathFull {
                path: Box::new(self.provenance.path.clone()),
            });
        }
        self.provenance.path.push(hop);
        Ok(self)
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
        let path = PathSet::try_from_hops(&[origin(3), origin(1)]).expect("two distinct hops");
        assert_eq!(path.hops(), &[origin(3), origin(1)]);
        assert_eq!(path.len(), 2);
        assert_eq!(path.last(), Some(origin(1)));
    }

    fn envelope(provenance: Provenance) -> Envelope {
        Envelope {
            station: StationKey::new("backyard"),
            observed_at: chrono::Utc::now(),
            received_at: chrono::Utc::now(),
            provenance,
            reading: Arc::new(Reading::default()),
        }
    }

    #[test]
    fn forwarding_appends_the_hop_and_leaves_origin_and_seq_alone() {
        let env = envelope(Provenance::admitted(origin(1), Seq::new(4711)));
        let fwd = env.forward(origin(2)).expect("2 is not on the path");
        assert_eq!(fwd.provenance.origin(), origin(1));
        assert_eq!(fwd.provenance.seq(), Seq::new(4711));
        assert_eq!(fwd.provenance.path().hops(), &[origin(1), origin(2)]);
        assert_eq!(fwd.provenance.path().last(), Some(origin(2)));
    }

    /// W7 §2.2's cycle, as a type error rather than a filter. The value a loop would need
    /// cannot be constructed, which is the whole of the structural claim.
    #[test]
    fn a_cycle_is_unrepresentable_rather_than_detected() {
        let a = envelope(Provenance::admitted(origin(1), Seq::FIRST));
        let b = a.forward(origin(2)).expect("A -> B");
        let c = b.forward(origin(3)).expect("B -> C");
        let refused = c.forward(origin(1)).expect_err("C -> A closes the cycle");
        match refused {
            ForwardRefused::Loop { node, path } => {
                assert_eq!(node, origin(1));
                assert_eq!(path.hops(), &[origin(1), origin(2), origin(3)]);
            }
            other => panic!("expected a loop refusal, got {other:?}"),
        }
    }

    /// A full path is refused as `PathFull`, **not** as a loop: W7 D-41 makes
    /// `uplink_loop_refused_total` an assertion that this code has a bug, so a deep
    /// topology must not be able to move it.
    #[test]
    fn a_ninth_hop_is_refused_as_a_full_path_and_not_as_a_loop() {
        let mut env = envelope(Provenance::admitted(origin(1), Seq::FIRST));
        for n in 2..=8 {
            env = env.forward(origin(n)).expect("eight hops are legal");
        }
        assert_eq!(env.provenance.path().len(), PathSet::MAX_HOPS);
        let refused = env
            .forward(origin(9))
            .expect_err("a ninth hop is one too many");
        assert!(
            matches!(refused, ForwardRefused::PathFull { .. }),
            "got {refused:?}"
        );
    }

    #[test]
    fn a_sealed_envelope_keeps_the_provenance_it_arrived_with() {
        let p = Provenance::sealed(origin(1), Seq::new(901), &[origin(1), origin(2)])
            .expect("a well-formed path");
        assert_eq!(p.origin(), origin(1));
        assert_eq!(p.seq(), Seq::new(901));
        assert_eq!(p.path().hops(), &[origin(1), origin(2)]);
    }

    #[test]
    fn sealing_rejects_the_three_paths_an_honest_encoder_cannot_produce() {
        assert_eq!(
            Provenance::sealed(origin(1), Seq::FIRST, &[]),
            Err(PathProblem::Empty)
        );
        assert_eq!(
            Provenance::sealed(origin(1), Seq::FIRST, &[origin(2), origin(1)]),
            Err(PathProblem::OriginNotFirst { origin: origin(1) }),
            "path[0] == origin is W7 D-6"
        );
        assert_eq!(
            Provenance::sealed(origin(1), Seq::FIRST, &[origin(1), origin(2), origin(1)]),
            Err(PathProblem::Duplicate { node: origin(1) })
        );
        let nine: Vec<_> = (1..=9).map(origin).collect();
        assert_eq!(
            Provenance::sealed(origin(1), Seq::FIRST, &nine),
            Err(PathProblem::TooLong { len: 9 })
        );
    }

    #[test]
    fn a_refusal_names_the_path_it_refused() {
        let env = envelope(Provenance::admitted(origin(0x0a), Seq::FIRST));
        let refused = env.forward(origin(0x0a)).expect_err("its own origin");
        assert!(
            refused.to_string().contains(&origin(0x0a).to_string()),
            "{refused}"
        );
    }
}
