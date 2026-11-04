//! `Ingress::admit` and `Ingress::accept` over the spool.
//!
//! # `admit`, in the order W3 §2.6 fixes
//!
//! ```text
//! 1. resolve(meta) -> StationKey | Reject            W2a §4a.6, the single gate
//! 2. for each PartialReading: merge into a Reading; enrich                (M3-3)
//! 3. seq = the next spool append position                                 W7 D-4
//! 4. envelope = Envelope { station, observed_at, received_at, provenance, reading }
//! 5. spool.append(&envelopes)                        group commit; the atom
//! 6. watch.send_replace(newest); broadcast.send(each)       after the commit, never before
//! 7. mw_readings_ingested_total += n
//! ```
//!
//! **Step 1 before step 4 is the whole of W2a's structural authentication claim.** There is
//! no `Envelope` without a `StationKey`, and the only producer of a `StationKey` is
//! [`StationRegistry::resolve`]. The Go gRPC receiver does the exact opposite —
//! `station.go:174` pushes into the distributor and `:194` consults the registry
//! afterwards, debug-logging an unknown sender — so an unregistered peer's readings are
//! stored and then noticed.
//!
//! **W3 D-87 is a property of the signatures here, not a rule to remember.** The only
//! `.await` on `admit`'s path is the spool's group commit: `resolve` is a synchronous trait
//! method over a published snapshot, the merge and the enrich are pure, and the `Latest`
//! and `Lossy` notifications are a `watch` and a `broadcast` that never wait for a
//! receiver. There is nothing in this file that *could* wait on a sink, a database, a peer
//! or a subscriber.
//!
//! # What is deliberately not here
//!
//! Three of W7 §4.4's eleven `accept` steps and one of `admit`'s are other items', and each
//! is marked at the point it would go rather than only listed here, because a comment in a
//! source file is not a work item (docs/reconciliation.md G-5) and a list in a module doc
//! is not either:
//!
//! - **enrich** (`admit` step 2, `accept` step 8) — M3-3, which owns **R1** and the rule
//!   that `enrich` writes a metric only where the value is `None`.
//! - **`origin ∈ peer.origins`** (`accept` step 3) — M6-4, which owns the peer registry and
//!   the `relay = false` default.
//! - **the clock-skew window** (`accept` step 4) — M6-4, with the rest of §4.6's boundary
//!   validation.
//! - **`station_key` resolves in `StationRegistry`** (`accept` step 5) — M6-4. The handle's
//!   one method takes a `FrameMeta`, which a forwarded envelope does not have; the lookup a
//!   peer's envelope needs is by key, and the item that adds that method is the item that
//!   needs it.
//! - **the `config.stations` upsert** (**D-56**) — M3-3, which that item's text carries.

use std::sync::Arc;

use mw_component::{
    AcceptError, Accepted, AdmitError, Admitted, FrameMeta, Ingress, Reject, StationRegistry,
};
use mw_types::{Classify, DisplayChain, Envelope, OriginId, PartialReading, PathSet};
use tracing::{error, warn};

use crate::metrics as m;
use crate::spool::{Draft, Spool, SpoolError, Verdict};

/// How many envelopes one `accept` may carry (W7 §4.1).
pub const MAX_BATCH_ENVELOPES: usize = 256;

/// How many encoded bytes one `accept` may carry (W7 §4.1).
pub const MAX_BATCH_BYTES: usize = 1024 * 1024;

/// The `Ingress` the daemon runs on.
///
/// Holds the spool and the station registry handle, both resolved once. It does **not**
/// hold a configuration reader: W3 **D-87** requires identity resolution to read a
/// published snapshot and never the database, and the Go collector's `LoadConfig()` **on
/// every single reading** is what that decision exists against.
pub struct SpoolIngress {
    spool: Arc<Spool>,
    registry: Arc<dyn StationRegistry>,
}

impl std::fmt::Debug for SpoolIngress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpoolIngress")
            .field("spool", &self.spool)
            .finish_non_exhaustive()
    }
}

impl SpoolIngress {
    /// The ingress over one spool and one station registry.
    pub fn new(spool: Arc<Spool>, registry: Arc<dyn StationRegistry>) -> Self {
        Self { spool, registry }
    }

    /// The spool behind it, for the fan-out and for a test.
    pub fn spool(&self) -> &Arc<Spool> {
        &self.spool
    }

    async fn admit_inner(
        &self,
        batch: &[PartialReading],
        meta: &FrameMeta,
    ) -> Result<Admitted, AdmitError> {
        // 1. The single gate. No envelope exists before this line returns `Ok`.
        let station = self.registry.resolve(meta).map_err(|reason| {
            m::rejected(reason, 1);
            warn!(
                component = %meta.receiver,
                reason = reason.as_str(),
                source = %meta.source,
                "a frame was refused by identity resolution and nothing was stored"
            );
            AdmitError::Rejected(reason)
        })?;

        // 2-4. Merge, enrich (M3-3), and seal. The `seq` is minted inside the spool's
        // critical section, because it IS the record's position (W7 D-4).
        let mut drafts = Vec::with_capacity(batch.len());
        let mut rejected = 0u32;
        for partial in batch {
            let observed_at = partial.observed_at.unwrap_or(meta.received_at);
            let mut reading = partial.values.clone();
            reading.time = observed_at;
            reading.station_id = station.as_str().to_string();
            // M3-3's enrich stage goes here: pure, idempotent, writing a metric only where
            // the value is `None` (W3 §2.5 consequence 3) -- which is what makes a
            // forwarded envelope safe across a hop. Until it exists, a reading reaches the
            // spool exactly as the codec produced it, which is correct and incomplete.
            if !carries_a_metric(&reading) {
                // W7 §4.6: an all-`None` reading becomes an all-`NULL` row the aggregates
                // still have to process. Counted per reading, and the batch still commits,
                // because one empty frame among four is not a reason to lose the other
                // three.
                m::rejected(Reject::EmptyReading, 1);
                rejected += 1;
                continue;
            }
            drafts.push(Draft {
                station: station.clone(),
                observed_at,
                received_at: meta.received_at,
                reading: Arc::new(reading),
            });
        }

        // 5-6. The append, then the notifications.
        let committed = self
            .spool
            .admit_local(&drafts)
            .await
            .map_err(spool_failed)?;
        let accepted = u32::try_from(committed.envelopes.len()).unwrap_or(u32::MAX);
        // 7.
        if accepted > 0 {
            m::ingested(station.as_str(), u64::from(accepted));
        }
        Ok(Admitted {
            accepted,
            rejected,
            seq_high: committed.seq_high,
        })
    }

    async fn accept_inner(
        &self,
        batch: &[Envelope],
        from: OriginId,
    ) -> Result<Accepted, AcceptError> {
        // 1. The batch bounds. A batch over the limit closes the stream rather than being
        // trimmed: trimming would make `IngestAck.through` a claim about envelopes the
        // collector never saw.
        if batch.len() > MAX_BATCH_ENVELOPES {
            m::rejected(Reject::BatchTooLarge, 1);
            return Err(AcceptError::Rejected(Reject::BatchTooLarge));
        }

        let me = self.spool.node_id();
        let mut fresh = Vec::with_capacity(batch.len());
        let mut duplicates = 0u32;
        let mut rejected = 0u32;
        for envelope in batch {
            // 2. The receive check (W7 D-7). Three of its six clauses are already
            // discharged by the type -- `Provenance::sealed` refuses a path that is empty,
            // over-long, repeating or not starting at its origin -- and the three
            // positional ones need to know who "me" and "the peer" are, so they are here.
            if let Err(reason) = receive_check(envelope.provenance.path(), me, from) {
                m::rejected(reason, 1);
                rejected += 1;
                continue;
            }
            // 3. `origin ∈ peer.origins` -- M6-4's, with the peer registry.
            // 4. The clock-skew window -- M6-4's, with §4.6's boundary validation.
            // 5. `station_key` resolves -- M6-4's, with the registry method it needs.

            // 6-7. Intern the origin (bounded by MAX_ORIGINS) and take the dedup verdict,
            // in one critical section. The gate is in front of the spool rather than
            // behind the database (W7 D-27): a `UNIQUE` violation fires after fan-out, by
            // which time the duplicate has already reached the downstream peers and the
            // live stream.
            let origin = envelope.provenance.origin();
            match self
                .spool
                .record_accepted(origin, envelope.provenance.seq())
            {
                Verdict::OriginTableFull => {
                    m::rejected(Reject::OriginTableFull, 1);
                    rejected += 1;
                    error!(
                        origin = %origin,
                        peer = %from,
                        max_origins = crate::spool::MAX_ORIGINS,
                        "the spool's origin table is full, so this envelope is dropped. \
                         Either the fleet is far larger than this node is configured for, \
                         or a peer is sending node ids it is inventing."
                    );
                    continue;
                }
                Verdict::Duplicate => {
                    duplicates += 1;
                    continue;
                }
                Verdict::FreshAfterOverrun { skipped } => {
                    m::window_overrun(skipped);
                    warn!(
                        origin = %origin,
                        peer = %from,
                        skipped,
                        "a peer's sequence number jumped past the dedup window, so {skipped} \
                         positions are no longer deduplicated here and rely on the \
                         writer's upsert"
                    );
                }
                Verdict::Fresh => {}
            }

            // 8. enrich -- M3-3's. Idempotent and `None`-only, which is what makes a
            // collector agree with its satellite rather than recompute from a different
            // formula version.
            fresh.push(envelope.clone());
        }

        if duplicates > 0 {
            m::duplicates(u64::from(duplicates));
        }

        // 9-10. One append call, one `fsync`, one atom; then the notifications.
        let committed = self
            .spool
            .append_sealed(&fresh)
            .await
            .map_err(accept_failed)?;
        for envelope in &committed.envelopes {
            m::ingested(envelope.station.as_str(), 1);
        }
        // 11.
        Ok(Accepted {
            accepted: u32::try_from(committed.envelopes.len()).unwrap_or(u32::MAX),
            duplicates,
            rejected,
        })
    }
}

impl Ingress for SpoolIngress {
    fn admit<'a>(
        &'a self,
        batch: &'a [PartialReading],
        meta: &'a FrameMeta,
    ) -> mw_component::BoxFuture<'a, Result<Admitted, AdmitError>> {
        Box::pin(self.admit_inner(batch, meta))
    }

    fn accept<'a>(
        &'a self,
        batch: &'a [Envelope],
        from: OriginId,
    ) -> mw_component::BoxFuture<'a, Result<Accepted, AcceptError>> {
        Box::pin(self.accept_inner(batch, from))
    }
}

/// The three positional clauses of W7 **D-7**'s six-clause receive check.
///
/// The other three are discharged by [`mw_types::Provenance::sealed`], which is the only
/// way a decoded envelope can exist: length ≤ 8, `path[0] == origin`, and all elements
/// distinct. These three need an identity:
///
/// 1. **addressed to me** — `path.last() == me`. The sender calls `forward(me)` before
///    sending, so the hop it is addressed to is the last element (W7 **D-6**).
/// 2. **I am not an interior hop** — `me ∉ path[..len - 1]`. This is the receive-side loop
///    guard, the second of the two independent ones; the first is
///    [`mw_types::Envelope::forward`]'s refusal, which is a property of a type with no
///    public constructor.
///
///    **It cannot currently fire, and that is worth stating rather than discovering.**
///    W7 D-7's sixth clause is "all elements distinct", and `Provenance::sealed` enforces
///    it at the decode — so a path whose last element is this node *and* which names this
///    node earlier is a path with a repeat, and no `Envelope` carrying one can be
///    constructed from bytes at all. The two clauses overlap once the path type enforces
///    distinctness. The check stays because it is the clause W7 names and because its cost
///    is two comparisons; what discharges it is asserted by
///    `a_path_that_names_this_node_twice_is_refused_before_the_receive_check`.
/// 3. **the sender appended itself** — `path[len - 2] == peer`. Without it a peer could
///    present a path that does not record the route the envelope actually took, which is
///    the forgery an ordered path exists to make visible.
///
/// A one-element path is addressed to its own origin, which is a locally admitted envelope
/// and not something a peer can send: it fails clause 1 unless the peer *is* this node, and
/// clause 3 has no `path[len - 2]` to check.
pub fn receive_check(path: &PathSet, me: OriginId, peer: OriginId) -> Result<(), Reject> {
    let hops = path.hops();
    if hops.last() != Some(&me) {
        return Err(Reject::NotAddressedHere);
    }
    let interior = &hops[..hops.len() - 1];
    if interior.contains(&me) {
        return Err(Reject::Loop);
    }
    match interior.last() {
        Some(sender) if *sender == peer => Ok(()),
        // Either the path is `[me]` alone, or the hop before me is somebody else. Both say
        // the path does not record the route this envelope took.
        _ => Err(Reject::PathForged),
    }
}

/// Whether a reading carries anything at all (W7 §4.6's last check).
///
/// Walks the registry rather than naming fields, so a metric added to
/// `metrics/registry.toml` is covered without an edit here. `Reading::get` returns `None`
/// for the eleven text metrics and the four timestamp ones, so those are asked about
/// separately — an envelope whose only content is `tz` or `stormStart` is still content.
fn carries_a_metric(reading: &mw_types::Reading) -> bool {
    mw_types::METRICS
        .iter()
        .any(|meta| reading.get(meta.key).is_some())
        || reading.tz.is_some()
        || reading.storm_start.is_some()
        || reading.lightning_time.is_some()
        || reading.sunrise.is_some()
        || reading.sunset.is_some()
}

/// A spool failure on the `admit` path, classified where the design classifies it.
fn spool_failed(e: SpoolError) -> AdmitError {
    match e {
        SpoolError::Full {
            depth_bytes,
            ceiling_bytes,
        } => {
            m::rejected(Reject::BatchTooLarge, 0);
            m::dropped_spool_full(1);
            // Logged once a minute by the dedupe layer (§5.6), not once per batch: at the
            // ceiling every station is failing and the per-batch line would be the only
            // thing in the log.
            error!(
                depth_bytes,
                ceiling_bytes,
                "the spool is at its ceiling, so this batch is dropped at the edge. \
                 Every station is now failing. Raise [spool].ceiling, or find the cursor \
                 that is not draining -- mw_spool_lag_envelopes names it."
            );
            AdmitError::SpoolFull { depth_bytes }
        }
        other => {
            error!(
                err = %DisplayChain(&other),
                class = other.class().as_str(),
                "the spool could not be written. A daemon that cannot durably accept \
                 readings is wrong rather than degraded (W2a Decision 62), so this is \
                 fatal and the process will exit non-zero."
            );
            AdmitError::spool(other)
        }
    }
}

/// The same, on the `accept` path. Two error enums rather than one, because a batch over
/// the size limit closes a stream and there is no stream on a station transport.
fn accept_failed(e: SpoolError) -> AcceptError {
    match e {
        SpoolError::Full {
            depth_bytes,
            ceiling_bytes,
        } => {
            m::dropped_spool_full(1);
            error!(
                depth_bytes,
                ceiling_bytes,
                "the spool is at its ceiling, so this peer's batch is refused with \
                 ACK_RESOURCE_EXHAUSTED. Its `through` does not advance, so nothing is \
                 lost on its side."
            );
            AcceptError::SpoolFull { depth_bytes }
        }
        other => {
            error!(
                err = %DisplayChain(&other),
                class = other.class().as_str(),
                "the spool could not be written (W2a Decision 62); this is fatal"
            );
            AcceptError::spool(other)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> OriginId {
        OriginId::new([n; 16])
    }

    fn path(hops: &[OriginId]) -> PathSet {
        PathSet::try_from_hops(hops).expect("a well-formed path")
    }

    /// A → C direct. `admit` at A set `path = [A]`; A's uplink `forward(C)` made it
    /// `[A, C]`.
    #[test]
    fn a_leaf_satellites_envelope_passes_the_receive_check() {
        let a = node(1);
        let c = node(3);
        assert_eq!(receive_check(&path(&[a, c]), c, a), Ok(()));
    }

    /// A → B → C. B's `forward(C)` appended C, so `path[len - 2]` is B, which is the peer
    /// C is talking to -- not A, which is the origin.
    #[test]
    fn a_relayed_envelope_is_checked_against_the_sender_and_not_the_origin() {
        let (a, b, c) = (node(1), node(2), node(3));
        assert_eq!(receive_check(&path(&[a, b, c]), c, b), Ok(()));
        assert_eq!(
            receive_check(&path(&[a, b, c]), c, a),
            Err(Reject::PathForged),
            "A is the origin, not the sender; C's peer on this stream is B"
        );
    }

    #[test]
    fn an_envelope_addressed_elsewhere_is_refused() {
        let (a, b, c) = (node(1), node(2), node(3));
        assert_eq!(
            receive_check(&path(&[a, b]), c, a),
            Err(Reject::NotAddressedHere)
        );
    }

    /// The receive-side half of the loop guard, and where it actually lands: a path that
    /// names this node twice cannot be decoded into an `Envelope`, so the `Loop` clause of
    /// `receive_check` is a belt to the type's braces rather than the thing that fires.
    ///
    /// Asserted at the boundary that does the work, because a test that reached into
    /// `receive_check` with a hand-built `PathSet` would be asserting a property of a value
    /// no decoder can produce.
    #[test]
    fn a_path_that_names_this_node_twice_is_refused_before_the_receive_check() {
        let (a, b, c) = (node(1), node(2), node(3));
        assert_eq!(
            PathSet::try_from_hops(&[c, a, b, c]).expect_err("a repeat is not a path"),
            mw_types::PathProblem::Duplicate { node: c }
        );
        assert_eq!(
            mw_types::Provenance::sealed(c, mw_types::Seq::FIRST, &[c, a, b, c])
                .expect_err("and so no envelope can carry one"),
            mw_types::PathProblem::Duplicate { node: c }
        );
        // The clause itself, over the only shape that reaches it -- a well-formed path.
        assert_eq!(receive_check(&path(&[a, b, c]), c, b), Ok(()));
    }

    #[test]
    fn a_locally_shaped_path_cannot_arrive_from_a_peer() {
        let a = node(1);
        assert_eq!(
            receive_check(&path(&[a]), a, node(2)),
            Err(Reject::PathForged),
            "there is no hop before the origin for the sender to be"
        );
    }

    #[test]
    fn an_empty_reading_carries_no_metric_and_a_text_only_one_does() {
        assert!(!carries_a_metric(&mw_types::Reading::default()));
        assert!(carries_a_metric(&mw_types::Reading {
            barometer: Some(29.92),
            ..mw_types::Reading::default()
        }));
        assert!(
            carries_a_metric(&mw_types::Reading {
                tz: Some("America/Denver".into()),
                ..mw_types::Reading::default()
            }),
            "`Reading::get` is None for a text metric, which is why it is asked separately"
        );
    }
}
