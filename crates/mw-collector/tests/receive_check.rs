//! The **six-clause receive check** (W7 **D-7**), one test per clause, plus the two
//! properties the clauses exist to establish.
//!
//! > On accept, an envelope is rejected unless `path` contains the sending peer and does
//! > not contain this node.
//!
//! The naive reading of that is wrong and the design says so: `forward(B)` inserts **the
//! destination**, so by the time B receives the envelope the path already contains B, and
//! "reject if the path contains me" rejects everything. What B must establish is that the
//! envelope has not been here *before*, and that is not a membership test — which is why
//! §2.1 makes the path ordered and the check positional.
//!
//! ```text
//! accept check, per envelope, at node S:
//!   1. path.len() in 1..=MAX_PATH                 else Reject::PathTooLong
//!   2. path[0] == origin                          else Reject::MalformedPath
//!   3. path.last() == S.node_id                   else Reject::NotAddressedHere
//!   4. path[..len-1] does not contain S.node_id   else Reject::Loop        ← the loop guard
//!   5. path[len-2] == peer.node_id  (len >= 2)    else Reject::PathForged
//!   6. all elements distinct                      else Reject::MalformedPath
//! ```
//!
//! **Three of the six are discharged by the decode and three by the check**, and that split
//! is the point rather than an implementation detail: 1, 2 and 6 are properties of
//! `mw_types::Provenance::sealed`, which is the only way a decoded envelope can exist — so
//! a peer cannot send bytes that produce an `Envelope` violating them, and the refusal
//! happens before anything in `mw-collector` runs. 3, 4 and 5 need to know who "me" and
//! "the peer" are, so they are `mw_pipeline::receive_check`'s.
//!
//! Both halves are asserted here, from the outside, because the claim being tested is
//! "every malformed path is rejected" and that is a claim about the two together.

use mw_component::Reject;
use mw_proto::wire::collector::v1 as pb;
use mw_types::{Envelope, ForwardRefused, OriginId, PathSet, Provenance, Seq};

/// Node `9` is this node throughout; `1` is the sending peer.
fn node(b: u8) -> OriginId {
    OriginId::new([b; 16])
}

const ME: u8 = 9;
const PEER: u8 = 1;

/// A wire envelope with `path` exactly as given — including paths no honest encoder
/// produces, which is the whole point.
///
/// Built as the **wire** message rather than as a domain `Envelope`, because three of the
/// six clauses are enforced at the decode and a test that started from a domain envelope
/// could not express a violation of them.
fn wire(origin: u8, seq: u64, path: &[u8]) -> pb::Envelope {
    pb::Envelope {
        origin: prost::bytes::Bytes::copy_from_slice(node(origin).as_bytes()),
        seq,
        path: path
            .iter()
            .map(|b| prost::bytes::Bytes::copy_from_slice(node(*b).as_bytes()))
            .collect(),
        station_key: "satellite-a:backyard".to_string(),
        observed_at: 1_762_732_800_000,
        received_at: 1_762_732_800_000,
        reading: Some(mw_proto::wire::Reading {
            station_id: "satellite-a:backyard".to_string(),
            // `readingTimestamp` is required: the domain type has no way to say "no time"
            // and substituting one would write a row at the Unix epoch under a real
            // station key (`mw_proto::WireError::MissingTime`).
            reading_timestamp: Some(mw_proto::wire::Timestamp {
                seconds: 1_762_732_800,
                nanos: 0,
            }),
            barometer: Some(29.92),
            ..Default::default()
        }),
    }
}

/// What the two halves of the check together say about one wire envelope.
///
/// `Err(None)` is "the decode refused it", which is the three structural clauses, and
/// `Err(Some(reason))` is the positional check's answer.
fn check(envelope: &pb::Envelope) -> Result<(), Option<Reject>> {
    let decoded = Envelope::try_from(envelope).map_err(|_| None)?;
    mw_pipeline::receive_check(decoded.provenance.path(), node(ME), node(PEER)).map_err(Some)
}

// ---------------------------------------------------------------------------------------
// The six clauses
// ---------------------------------------------------------------------------------------

/// The chain of §2.1, step 3: every clause passes.
#[test]
fn clause_0_a_well_formed_hop_is_accepted() {
    assert_eq!(check(&wire(PEER, 4711, &[PEER, ME])), Ok(()));
}

#[test]
fn clause_1_a_path_longer_than_max_path_is_refused_at_the_decode() {
    // Nine hops, where `MAX_PATH` is eight.
    let path: Vec<u8> = (10..18).chain(std::iter::once(ME)).collect();
    assert_eq!(path.len(), 9);
    assert_eq!(
        check(&wire(10, 1, &path)),
        Err(None),
        "a path over the bound cannot be decoded into an Envelope at all"
    );
    assert_eq!(PathSet::MAX_HOPS, 8, "W7 D-8's bound");
    // Eight is inside it, so the refusal is the bound and not the length.
    let eight: Vec<u8> = (10..17).chain(std::iter::once(ME)).collect();
    assert_eq!(eight.len(), 8);
    assert!(Envelope::try_from(&wire(10, 1, &eight)).is_ok());
}

#[test]
fn clause_1_an_empty_path_is_refused_at_the_decode() {
    assert_eq!(check(&wire(PEER, 1, &[])), Err(None));
}

#[test]
fn clause_2_a_path_that_does_not_start_at_its_origin_is_refused_at_the_decode() {
    // `origin` says node 2 and `path[0]` says node 1.
    assert_eq!(
        check(&wire(2, 1, &[PEER, ME])),
        Err(None),
        "`path[0] == origin` is a property of `Provenance::sealed`"
    );
}

#[test]
fn clause_3_an_envelope_addressed_somewhere_else_is_not_addressed_here() {
    // Addressed to node 8, delivered to node 9.
    assert_eq!(
        check(&wire(PEER, 1, &[PEER, 8])),
        Err(Some(Reject::NotAddressedHere))
    );
}

/// Clause 4 is the receive-side loop guard, and the one whose overlap with clause 6 is
/// worth asserting rather than assuming: a path whose last element is this node **and**
/// which names this node earlier is a path with a repeat, so the decode refuses it first.
#[test]
fn clause_4_a_path_that_names_this_node_twice_is_refused_before_the_positional_check() {
    assert_eq!(
        check(&wire(ME, 1, &[ME, PEER, ME])),
        Err(None),
        "clause 6 fires at the decode, which is why clause 4 cannot currently be reached"
    );
}

#[test]
fn clause_5_a_path_whose_sender_did_not_append_itself_is_forged() {
    // The hop before `me` is node 2, and node 1 is the peer that sent it.
    assert_eq!(
        check(&wire(2, 1, &[2, 2 + 1, ME])),
        Err(Some(Reject::PathForged)),
        "the path does not record the route this envelope actually took"
    );
}

/// A one-element path is a locally admitted envelope, which is not something a peer can
/// send: it fails clause 3 unless the peer *is* this node, and clause 5 has no
/// `path[len - 2]` to check.
#[test]
fn clause_5_a_single_hop_path_from_a_peer_is_refused() {
    assert_eq!(
        check(&wire(PEER, 1, &[PEER])),
        Err(Some(Reject::NotAddressedHere))
    );
    // …and even addressed here, it names no sender.
    assert_eq!(check(&wire(ME, 1, &[ME])), Err(Some(Reject::PathForged)));
}

#[test]
fn clause_6_a_repeated_hop_is_refused_at_the_decode() {
    assert_eq!(check(&wire(PEER, 1, &[PEER, 2, 2, ME])), Err(None));
}

/// A forged cycle: a non-conforming peer presents `[A, B, A]` to A. §2.2 step 5‴.
#[test]
fn a_forged_cycle_is_refused() {
    // At node 9: `[9, 1, 9]` — this node, the peer, this node again.
    assert_eq!(
        check(&wire(ME, 1, &[ME, PEER, ME])),
        Err(None),
        "the repeat is caught at the decode; a peer cannot construct this envelope here"
    );
}

/// The decode's other refusals, which are §4.6's and are the same class of defence: an
/// envelope that is structurally impossible never becomes an `Envelope`.
#[test]
fn the_decode_refuses_an_envelope_with_no_station_no_reading_or_a_bad_node_id() {
    let mut no_station = wire(PEER, 1, &[PEER, ME]);
    no_station.station_key = String::new();
    assert_eq!(check(&no_station), Err(None));

    let mut no_reading = wire(PEER, 1, &[PEER, ME]);
    no_reading.reading = None;
    assert_eq!(check(&no_reading), Err(None));

    let mut short_origin = wire(PEER, 1, &[PEER, ME]);
    short_origin.origin = prost::bytes::Bytes::from_static(&[1, 2, 3]);
    assert_eq!(check(&short_origin), Err(None));

    let mut short_hop = wire(PEER, 1, &[PEER, ME]);
    short_hop.path[0] = prost::bytes::Bytes::from_static(&[1, 2, 3]);
    assert_eq!(check(&short_hop), Err(None));
}

// ---------------------------------------------------------------------------------------
// The two properties, exhaustively
// ---------------------------------------------------------------------------------------

/// Every path over a four-node alphabet, up to the hop bound.
///
/// **Exhaustive rather than sampled**, which is the stronger form of the property test the
/// plan asks for and needs no dependency: the interesting structure is "does this hop
/// already appear", and over a small alphabet every arrangement that can exhibit it is
/// enumerable. 4 nodes to length 5 is 1,364 paths, which runs in milliseconds, and a
/// random sampler over the same space would only ever check a subset of it.
fn every_path(alphabet: u8, max_len: usize) -> Vec<Vec<OriginId>> {
    let mut out: Vec<Vec<OriginId>> = Vec::new();
    let mut frontier: Vec<Vec<OriginId>> = (0..alphabet).map(|b| vec![node(b)]).collect();
    out.extend(frontier.iter().cloned());
    for _ in 1..max_len {
        let mut next = Vec::new();
        for path in &frontier {
            for b in 0..alphabet {
                let mut longer = path.clone();
                longer.push(node(b));
                next.push(longer);
            }
        }
        out.extend(next.iter().cloned());
        frontier = next;
    }
    out
}

/// **Property 1: a relay cycle is unrepresentable, not filtered.**
///
/// `Envelope::forward(hop)` is the only way to produce an envelope addressed at a peer, and
/// it refuses when `hop` is already on the path. So for every path this node could hold and
/// every hop it could forward to, the forwarded value exists **if and only if** the hop is
/// not on the path — which is the statement that a cycle cannot be built rather than that
/// it is caught.
#[test]
fn forwarding_to_a_node_already_on_the_path_is_unrepresentable() {
    let mut refused = 0usize;
    let mut built = 0usize;
    for path in every_path(4, PathSet::MAX_HOPS) {
        // Only the paths an honest encoder could produce: `sealed` is what makes the rest
        // unconstructable, and `every_path` deliberately enumerates those too so that the
        // filter below is the one asserting it.
        let Ok(provenance) = Provenance::sealed(path[0], Seq::FIRST, &path) else {
            continue;
        };
        let envelope = Envelope {
            station: mw_types::StationKey::new("satellite-a:backyard"),
            observed_at: chrono::DateTime::from_timestamp_millis(1_762_732_800_000)
                .expect("an instant"),
            received_at: chrono::DateTime::from_timestamp_millis(1_762_732_800_000)
                .expect("an instant"),
            provenance,
            reading: std::sync::Arc::new(mw_types::Reading::default()),
        };
        for hop in 0..4u8 {
            let hop = node(hop);
            let on_path = path.contains(&hop);
            let full = path.len() >= PathSet::MAX_HOPS;
            match envelope.clone().forward(hop) {
                Ok(forwarded) => {
                    assert!(
                        !on_path,
                        "forward({hop}) produced an envelope whose path already contained \
                         it: {path:?}"
                    );
                    assert!(!full, "forward past the hop bound: {path:?}");
                    // What `forward` produces is addressed to that hop, which is the
                    // clause the receiver checks.
                    assert_eq!(forwarded.provenance.path().last(), Some(hop));
                    assert_eq!(
                        forwarded.provenance.path().len(),
                        path.len() + 1,
                        "a hop is appended, never substituted"
                    );
                    assert_eq!(
                        forwarded.provenance.origin(),
                        path[0],
                        "forwarding never rewrites the origin"
                    );
                    built += 1;
                }
                Err(ForwardRefused::Loop {
                    node: refused_at, ..
                }) => {
                    assert!(on_path, "forward refused a hop that was not on the path");
                    assert_eq!(refused_at, hop);
                    refused += 1;
                }
                Err(ForwardRefused::PathFull { .. }) => {
                    assert!(full, "forward refused a path inside the bound: {path:?}");
                    refused += 1;
                }
            }
        }
    }
    assert!(built > 0 && refused > 0, "{built} built, {refused} refused");
}

/// **Property 2: an envelope this node has already handled cannot come back.**
///
/// Over the same enumeration, from the receiver's side: a path that passes every clause of
/// the receive check never has this node as an interior hop. The two guards are independent
/// — one is a property of a type with no public constructor, the other is this check — and
/// this is the second one stated as a property rather than as a case.
#[test]
fn no_accepted_path_has_this_node_as_an_interior_hop() {
    let mut accepted = 0usize;
    for path in every_path(4, PathSet::MAX_HOPS) {
        let Ok(provenance) = Provenance::sealed(path[0], Seq::FIRST, &path) else {
            continue;
        };
        // `me` and the peer are drawn from the same alphabet, so every (path, me, peer)
        // combination the topology admits is covered.
        for me in 0..4u8 {
            for peer in 0..4u8 {
                let (me, peer) = (node(me), node(peer));
                if mw_pipeline::receive_check(provenance.path(), me, peer).is_ok() {
                    let hops = provenance.path().hops();
                    assert_eq!(hops.last(), Some(&me), "clause 3");
                    assert!(
                        !hops[..hops.len() - 1].contains(&me),
                        "clause 4: {path:?} was accepted at an interior hop"
                    );
                    assert_eq!(hops[hops.len() - 2], peer, "clause 5");
                    accepted += 1;
                }
            }
        }
    }
    assert!(
        accepted > 0,
        "the check accepted nothing, so it proves nothing"
    );
}

/// **The two guards compose**: whatever one node forwards, the next node accepts — and
/// nothing else. A round trip over the enumeration, which is what says the sender's
/// `forward` and the receiver's check agree about what a path means.
#[test]
fn whatever_forward_produces_the_receiver_accepts() {
    for path in every_path(4, PathSet::MAX_HOPS - 1) {
        let Ok(provenance) = Provenance::sealed(path[0], Seq::FIRST, &path) else {
            continue;
        };
        let sender = *path.last().expect("every enumerated path is non-empty");
        let envelope = Envelope {
            station: mw_types::StationKey::new("satellite-a:backyard"),
            observed_at: chrono::DateTime::from_timestamp_millis(1).expect("an instant"),
            received_at: chrono::DateTime::from_timestamp_millis(1).expect("an instant"),
            provenance,
            reading: std::sync::Arc::new(mw_types::Reading::default()),
        };
        for hop in 0..4u8 {
            let me = node(hop);
            if let Ok(forwarded) = envelope.clone().forward(me) {
                assert_eq!(
                    mw_pipeline::receive_check(forwarded.provenance.path(), me, sender),
                    Ok(()),
                    "a hop the sender's own `forward` produced was refused by its \
                     receiver: path {path:?}, hop {me}"
                );
            }
        }
    }
}
