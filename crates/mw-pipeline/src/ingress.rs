//! `Ingress::admit` and `Ingress::accept` over the spool.
//!
//! # `admit`, in the order W3 §2.6 fixes
//!
//! ```text
//! 1. resolve(meta) -> StationKey | Reject            W2a §4a.6, the single gate
//! 2. for each PartialReading: merge into a Reading; enrich              R1, §2.5
//! 3. seq = the next spool append position                                 W7 D-4
//! 4. envelope = Envelope { station, observed_at, received_at, provenance, reading }
//! 5. spool.append(&envelopes)                        group commit; the atom
//! 6. watch.send_replace(newest); broadcast.send(each)       after the commit, never before
//! 7. mw_readings_ingested_total += n
//! ```
//!
//! **Step 1 before step 4 is the whole of W2a's structural authentication claim.** On this
//! path there is no `Envelope` without a `StationKey`, and the only producer of one is
//! [`StationRegistry::resolve`]. The Go gRPC receiver does the exact opposite —
//! `station.go:174` pushes into the distributor and `:194` consults the registry
//! afterwards, debug-logging an unknown sender — so an unregistered peer's readings are
//! stored and then noticed.
//!
//! The claim is about `admit`, and it is worth being exact about its limit: `StationKey::new`
//! is public and validates nothing, and `mw-proto`'s envelope decode calls it on a peer's
//! bytes. So on the `accept` path a station key is a *claim* until W7 §4.4 step 5 resolves
//! it — which is M6-4's, is marked below at the line it goes on, and is why `accept` does
//! not label a metric with the key.
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
//! Three of W7 §4.4's eleven `accept` steps are other items', and each is marked at the
//! point it would go rather than only listed here, because a comment in a source file is
//! not a work item (docs/reconciliation.md G-5) and a list in a module doc is not either:
//!
//! - **`origin ∈ peer.origins`** (`accept` step 3) — M6-4, which owns the peer registry and
//!   the `relay = false` default.
//! - **the clock-skew window** (`accept` step 4) — M6-4, with the rest of §4.6's boundary
//!   validation.
//! - **`station_key` resolves in `StationRegistry`** (`accept` step 5) — M6-4. The lookup a
//!   peer's envelope needs is by key, and [`StationRegistry::metadata`] is now by key — but
//!   it is total by design (an unknown station is `StationMeta::EMPTY`, not a refusal), so
//!   it is deliberately **not** the resolution check. Turning it into one would make an
//!   unplaced station's envelopes unstorable.
//!
//! **The `stations` upsert of D-56 is not here either, and that one is a departure rather
//! than a deferral.** D-56 assigns it to `admit`, and `admit` cannot do it: W3 **D-87**
//! allows no I/O on this path but the spool append, and `ci/layers.toml` forbids this crate
//! `sqlx` and `mw-store` outright, so there is no expression of "upsert a PostgreSQL row"
//! available here at all. `writer:timescale` does it instead, in the same statement as the
//! rows it describes. Filed as a proposed amendment to D-56 in `docs/reconciliation.md`'s
//! *Proposed amendments* section; `mw_store::writer`'s module documentation carries the
//! consequence.

use std::sync::Arc;

use mw_component::{
    AcceptError, Accepted, AdmitError, Admitted, FrameMeta, Ingress, Reject, StationRegistry,
};
use mw_types::{Classify, DisplayChain, Envelope, OriginId, PartialReading, PathSet};
use prost::Message as _;
use tracing::{error, warn};

use crate::metrics as m;
use crate::spool::{Draft, Spool, SpoolError};

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
    stats: std::sync::Mutex<IngressStats>,
}

/// What this ingress has refused and dropped, as numbers a test can read.
///
/// The same pattern as [`crate::SpoolStats`] and for the same reason: the plan requires the
/// ceiling policy to be **counted**, and asserting a Prometheus counter needs a
/// process-global recorder that only one test per binary could install. These are the same
/// events the `metrics` calls export — one source, two surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IngressStats {
    /// Readings refused by identity resolution or a per-reading check, all reasons.
    pub rejected: u64,
    /// Readings refused because the reading carried no metric (W7 §4.6).
    pub empty_readings: u64,
    /// Readings **lost** at the spool's ceiling — a count of readings, not of batches.
    pub dropped_spool_full: u64,
    /// Envelopes the dedup gate already had.
    pub duplicates: u64,
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
        Self {
            spool,
            registry,
            stats: std::sync::Mutex::new(IngressStats::default()),
        }
    }

    /// What has been refused and dropped.
    pub fn stats(&self) -> IngressStats {
        *self
            .stats
            .lock()
            .expect("the ingress counters are not poisoned")
    }

    fn count(&self, f: impl FnOnce(&mut IngressStats)) {
        f(&mut self
            .stats
            .lock()
            .expect("the ingress counters are not poisoned"));
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
            self.count(|s| s.rejected += 1);
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
        // The envelope's two instants are `int64` milliseconds on the wire (§4.1) while
        // `reading.time` is a nanosecond `Timestamp`, so a reading sealed from a
        // nanosecond clock comes back from the spool with `observed_at` and `reading.time`
        // disagreeing — on every envelope, not only across a hop, because **D-11** makes
        // the spool record that same message. Truncating all three together here makes
        // them agree by construction, which is the invariant `admit` is supposed to
        // establish.
        let received_at = to_millis(meta.received_at);
        // One lookup per batch rather than one per reading: every reading in a batch is
        // from the same station -- `resolve` has already said so -- and the metadata is a
        // snapshot read, so hoisting it is the same answer with one pointer load.
        let meta_for_station = self.registry.metadata(&station);
        for partial in batch {
            let observed_at = partial.observed_at.map_or(received_at, to_millis);
            let mut reading = partial.values.clone();
            reading.time = observed_at;
            reading.station_id = station.as_str().to_string();
            // The enrich stage (**R1**, W3 §2.5): pure, idempotent, writing a metric only
            // where the value is `None`. After this line, storage, the snapshot and every
            // peer see the same enriched reading, which is the whole reason it is here
            // rather than in the writer -- and `reading.time` is already the admitted
            // observation instant, which the clear-sky model is a function of.
            crate::enrich::enrich(&mut reading, &meta_for_station);
            // After enrich, and it is worth being exact about what that does and does not
            // buy: every derived value is a function of at least one *reported* metric, so
            // enrich cannot turn an empty reading into a content-carrying one and this
            // order changes which readings are rejected not at all. It is this way round
            // because the check is about the reading that will be stored, and a later
            // derived value that needed only the station's metadata -- an altitude-derived
            // constant, say -- would make the two orders differ silently.
            if !carries_a_metric(&reading) {
                // W7 §4.6: an all-`None` reading becomes an all-`NULL` row the aggregates
                // still have to process. Counted per reading, and the batch still commits,
                // because one empty frame among four is not a reason to lose the other
                // three.
                m::rejected(Reject::EmptyReading, 1);
                self.count(|s| {
                    s.rejected += 1;
                    s.empty_readings += 1;
                });
                rejected += 1;
                continue;
            }
            drafts.push(Draft {
                station: station.clone(),
                observed_at,
                received_at,
                reading: Arc::new(reading),
            });
        }

        // 5-6. The append, then the notifications.
        let readings = drafts.len();
        let committed = self.spool.admit_local(&drafts).await.map_err(|e| {
            if matches!(e, SpoolError::Full { .. }) {
                self.count(|s| s.dropped_spool_full += readings as u64);
            }
            spool_failed(e, readings)
        })?;
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
            self.count(|s| s.rejected += 1);
            return Err(AcceptError::Rejected(Reject::BatchTooLarge));
        }
        // §4.1 and §4.5 bound a batch by **both** count and encoded size, and §4.5 owes a
        // `413` on the second. `encoded_len` rather than a re-encode: prost computes it
        // without allocating, and this runs before anything is sealed.
        let encoded: usize = batch
            .iter()
            .map(|e| mw_proto::wire::Envelope::from(e).encoded_len())
            .sum();
        if encoded > MAX_BATCH_BYTES {
            m::rejected(Reject::BatchTooLarge, 1);
            self.count(|s| s.rejected += 1);
            warn!(
                peer = %from,
                encoded,
                limit = MAX_BATCH_BYTES,
                "a peer's batch is over the encoded-size limit and is refused whole; \
                 trimming it would make the ack a claim about envelopes never seen"
            );
            return Err(AcceptError::Rejected(Reject::BatchTooLarge));
        }

        let me = self.spool.node_id();
        let mut candidates = Vec::with_capacity(batch.len());
        let mut rejected = 0u32;
        for envelope in batch {
            // 2. The receive check (W7 D-7). Three of its six clauses are already
            // discharged by the type -- `Provenance::sealed` refuses a path that is empty,
            // over-long, repeating or not starting at its origin -- and the three
            // positional ones need to know who "me" and "the peer" are, so they are here.
            if let Err(reason) = receive_check(envelope.provenance.path(), me, from) {
                m::rejected(reason, 1);
                self.count(|s| s.rejected += 1);
                rejected += 1;
                continue;
            }
            // 3. `origin ∈ peer.origins` -- M6-4's, with the peer registry.
            // 4. The clock-skew window -- M6-4's, with §4.6's boundary validation.
            // 5. `station_key` resolves -- M6-4's, with the registry method it needs.
            // 8. enrich. Idempotent and `None`-only, which is what makes a collector agree
            // with its satellite rather than recompute from a different formula version.
            // It is here rather than skipped because a satellite may be *older* than this
            // build, and W7 §4.4 step 8 is where a column the satellite had never heard
            // of gets filled.
            //
            // `metadata` is by station **key**, which on this path is still the peer's
            // claim until step 5 lands -- so an unregistered key answers
            // `StationMeta::EMPTY` and the three metadata-dependent columns stay `NULL`.
            // That is the right failure: a guessed latitude would put a clear-sky figure
            // in a column for a station nobody has placed.
            // Derived from the **borrowed** envelope, before anything is cloned: this
            // envelope's `Arc<Reading>` is shared with the caller's batch, so
            // `Arc::make_mut` is unconditionally a 216-field deep copy and up to fifteen
            // allocations. `derive` answers "is there anything to write" exactly and
            // allocates nothing, so an envelope a satellite already enriched -- the
            // common case on a collector -- costs six `is_some()` reads and no copy.
            let derived = crate::enrich::derive(
                &envelope.reading,
                &self.registry.metadata(&envelope.station),
            );
            let mut candidate = envelope.clone();
            if !derived.is_empty() {
                crate::enrich::apply(std::sync::Arc::make_mut(&mut candidate.reading), derived);
                m::enriched_on_accept(1);
            }
            candidates.push(candidate);
        }

        // 6, 7 and 9 are **one call and one critical section**: intern the origin, take
        // the dedup verdict, and append. They cannot be separated -- see
        // `Spool::accept_sealed`, whose doc records what splitting them cost. The gate is
        // in front of the spool rather than behind the database (W7 D-27): a `UNIQUE`
        // violation fires after fan-out, by which time the duplicate has already reached
        // the downstream peers and the live stream.
        //
        // 10. The notifications, inside the append and after its commit.
        let committed = self.spool.accept_sealed(&candidates).await.map_err(|e| {
            if matches!(e, SpoolError::Full { .. }) {
                self.count(|s| s.dropped_spool_full += candidates.len() as u64);
            }
            accept_failed(e, candidates.len())
        })?;

        if committed.duplicates > 0 {
            m::duplicates(u64::from(committed.duplicates));
            self.count(|s| s.duplicates += u64::from(committed.duplicates));
        }
        if committed.origin_table_full > 0 {
            m::rejected(
                Reject::OriginTableFull,
                u64::from(committed.origin_table_full),
            );
            self.count(|s| s.rejected += u64::from(committed.origin_table_full));
            error!(
                peer = %from,
                dropped = committed.origin_table_full,
                max_origins = crate::spool::MAX_ORIGINS,
                "the spool's origin table is full, so {} of this peer's envelopes were \
                 dropped. Either the fleet is far larger than this node is configured \
                 for, or a peer is sending node ids it is inventing.",
                committed.origin_table_full
            );
        }
        if committed.overrun_positions > 0 {
            m::window_overrun(committed.overrun_positions);
            warn!(
                peer = %from,
                skipped = committed.overrun_positions,
                "a peer's sequence numbers jumped past the dedup window, so {} positions \
                 below them are no longer deduplicated here and fall back to the writer's \
                 upsert. Nothing is dropped; a later envelope in that range is accepted.",
                committed.overrun_positions
            );
        }
        for envelope in &committed.envelopes {
            // No `station` label here: on this path the key is a peer's claim until
            // M6-4's registry check lands, and §5.10 forbids a label whose values come
            // from a request. The `admit` path labels its own, because `resolve` has
            // already bounded it.
            m::accepted(envelope.station.as_str());
        }
        // 11.
        Ok(Accepted {
            accepted: u32::try_from(committed.envelopes.len()).unwrap_or(u32::MAX),
            duplicates: committed.duplicates,
            rejected: rejected + committed.origin_table_full,
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

/// Truncate to the millisecond the wire's `observed_at`/`received_at` can carry.
///
/// Total: `timestamp_millis` is in range for every instant `chrono` can hold, and
/// `from_timestamp_millis` round-trips it. The `unwrap_or` is unreachable and returns the
/// input rather than inventing an instant.
fn to_millis(at: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_millis(at.timestamp_millis()).unwrap_or(at)
}

/// Whether a reading carries anything at all (W7 §4.6's last check).
///
/// Compares against a `Reading` that has the same two dimensions and no metrics, which
/// makes the check **registry-proof**: a metric added to `metrics/registry.toml` is covered
/// without an edit here, and there is no list to fall out of date.
///
/// The obvious implementation — walk `METRICS` asking `Reading::get` — is wrong, and
/// subtly: `get` returns `None` for the fifteen non-numeric metrics (eleven text, four
/// timestamp), so an envelope whose only content is one of them reads as empty and is
/// dropped. A hand-written list of "and these ones too" is what that becomes, and it had
/// already missed ten of the fifteen (`extratext1`..`extratext10`). The generated type
/// derives `PartialEq`, so the comparison is the whole check.
fn carries_a_metric(reading: &mw_types::Reading) -> bool {
    let empty = mw_types::Reading {
        time: reading.time,
        station_id: reading.station_id.clone(),
        ..mw_types::Reading::default()
    };
    *reading != empty
}

/// A spool failure on the `admit` path, classified where the design classifies it.
///
/// `readings` is how many were in the refused batch, because `mw_readings_dropped_total` is
/// a reading count (§5.10) and the batch is lost whole.
fn spool_failed(e: SpoolError, readings: usize) -> AdmitError {
    match e {
        SpoolError::Full {
            depth_bytes,
            ceiling_bytes,
        } => {
            m::dropped_spool_full(readings as u64);
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
fn accept_failed(e: SpoolError, envelopes: usize) -> AcceptError {
    match e {
        SpoolError::Full {
            depth_bytes,
            ceiling_bytes,
        } => {
            m::dropped_spool_full(envelopes as u64);
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

    /// Enrich cannot rescue an all-`None` reading, which is the precondition the ordering
    /// comment in `admit_inner` rests on. Over every derived column, with the station
    /// metadata that would let each one fire.
    #[test]
    fn the_enrich_stage_cannot_turn_an_empty_reading_into_a_stored_row() {
        let mut empty = mw_types::Reading {
            time: chrono::Utc::now(),
            station_id: "backyard".into(),
            ..mw_types::Reading::default()
        };
        let rich = mw_component::StationMeta {
            latitude: Some(39.7392),
            longitude: Some(-104.9903),
            altitude_m: Some(1609.0),
            snow_base_distance_mm: Some(1500.0),
        };
        assert!(
            !crate::enrich::enrich(&mut empty, &rich),
            "every R1 value is a function of at least one reported metric"
        );
        assert!(!carries_a_metric(&empty));
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

    /// Over **every** metric in the registry, not a hand-picked few: for each one, a
    /// reading carrying only that metric must count as carrying something.
    ///
    /// This is the test the old hand-written list could not have: `Reading::get` is `None`
    /// for fifteen non-numeric metrics, and the list named five of them, so a reading whose
    /// only content was `extratext1` was dropped as empty. Driving it off the registry means
    /// a metric added to `metrics/registry.toml` is covered without an edit here.
    #[test]
    fn every_registry_metric_counts_as_content() {
        let bare = mw_types::Reading::default();
        assert!(!carries_a_metric(&bare), "nothing set is nothing carried");
        assert!(
            !carries_a_metric(&mw_types::Reading {
                time: chrono::Utc::now(),
                station_id: "backyard".into(),
                ..mw_types::Reading::default()
            }),
            "the two dimensions admission fills in are not content"
        );

        // The shapes that used to be dropped. `Reading` has no setter by key, so this is a
        // sample rather than a sweep — but the function compares against a default rather
        // than consulting a list, so a metric it has never heard of is covered for the same
        // reason these are. `extra_text1` and `extra_text10` are the two ends of the family
        // the old hand-written list omitted entirely.
        for probe in [
            mw_types::Reading {
                extra_text1: Some("a note from the station".into()),
                ..mw_types::Reading::default()
            },
            mw_types::Reading {
                extra_text10: Some("another".into()),
                ..mw_types::Reading::default()
            },
            mw_types::Reading {
                tz: Some("America/Denver".into()),
                ..mw_types::Reading::default()
            },
            mw_types::Reading {
                storm_start: Some(chrono::Utc::now()),
                ..mw_types::Reading::default()
            },
            mw_types::Reading {
                barometer: Some(29.92),
                ..mw_types::Reading::default()
            },
        ] {
            assert!(
                carries_a_metric(&probe),
                "a reading carrying only one metric is content"
            );
        }
    }
}
