//! The dedup gate of docs/collector.md §4.4: a persisted high water plus an in-memory
//! window of what arrived above it.
//!
//! **Why the gate is in front of the spool and not a database constraint** (W7 **D-27**).
//! A `UNIQUE` violation fires *after* fan-out, by which time the duplicate has already
//! reached the downstream peers, the snapshot and the live stream. The spool is the
//! fan-out point, so the only place a duplicate can be stopped before anyone sees it is
//! the append.
//!
//! **Why a high water is not enough on its own.** W7 §2.3's diamond — A sends to both B
//! and C, both send to D — delivers `A:4712` via B before `A:4711` via C. With only a high
//! water, "accepted" would have to mean "advances a contiguous run", so one envelope that
//! took the long way pins the water forever and every later one is re-accepted as new. The
//! window is what makes out-of-order arrival ordinary.
//!
//! **Why the window is in memory and the high water is not.** Losing the window costs at
//! most [`WINDOW`] deduplicated replays per origin, which the writer's
//! `ON CONFLICT … DO UPDATE` absorbs; persisting it would add a second fsynced structure
//! for no correctness gain. Losing the high water would make every envelope a peer ever
//! sent look new, so it is fsynced in the same group commit as the records (§3.3).

use std::collections::BTreeMap;

use mw_types::OriginId;

/// How many sequence numbers above the high water one origin's window tracks.
///
/// 4096 per W7 §4.4. At the measured ~2 envelopes/s across the whole fleet, that is over
/// half an hour of reordering tolerance for a single origin, which is far beyond any
/// plausible path-length difference in a topology of at most eight hops.
pub const WINDOW: u64 = 4096;

/// One origin's dedup state.
///
/// Two numbers, and keeping them separate is the whole correctness argument:
///
/// - `hw` — the **contiguous accepted prefix**. Everything at or below it *has been
///   accepted*, so `seq <= hw` is a sound `Duplicate`. It only ever advances over
///   positions a bit was actually set for.
/// - `base` — the lowest `seq` the bitmap can speak about. Bit `i` is `base + i`. Normally
///   `base == hw + 1`, so the window sits directly on top of the water.
///
/// They come apart when a peer's `seq` jumps further than `WINDOW` ahead. The window
/// slides so the new `seq` is its last slot, and `base` moves with it — but **`hw` does
/// not**, because nothing in the skipped range was accepted. Those positions become
/// *unspeakable*: `base > seq > hw` answers `Fresh`, because the only two choices are
/// "might be a duplicate, let the writer's upsert absorb it" and "might be a genuine
/// envelope, lose it", and W7 §4.4 picks the first — "losing the window costs at most
/// `WINDOW` deduplicated replays per origin, which the database backstop absorbs".
///
/// An earlier version advanced `hw` on that slide. That made `Duplicate` mean "below a
/// number" rather than "seen before", which is a silent, permanent, peer-triggerable drop
/// of real data: one envelope with `seq = u64::MAX` moved the water to the top of the
/// range and every later envelope from that origin was refused for ever, across restarts,
/// because `hw` is persisted in `MANIFEST`. The invariant that forbids it is the one
/// stated on `hw` above, and it is now asserted by
/// [`the_high_water_only_ever_covers_accepted_sequence_numbers`](tests::the_high_water_only_ever_covers_accepted_sequence_numbers).
#[derive(Debug, Clone)]
struct Window {
    hw: u64,
    /// The `seq` bit 0 represents. `base >= hw + 1` always.
    base: u64,
    /// `WINDOW` bits, covering `[base, base + WINDOW)`.
    bits: Vec<u64>,
}

impl Window {
    fn new(hw: u64) -> Self {
        Self {
            hw,
            base: hw + 1,
            bits: vec![0; (WINDOW / 64) as usize],
        }
    }

    fn get(&self, offset: u64) -> bool {
        let (word, bit) = ((offset / 64) as usize, offset % 64);
        self.bits[word] & (1 << bit) != 0
    }

    fn set(&mut self, offset: u64) {
        let (word, bit) = ((offset / 64) as usize, offset % 64);
        self.bits[word] |= 1 << bit;
    }

    /// Slide the window up by `by` positions. `base` moves; `hw` is the caller's business.
    fn slide(&mut self, by: u64) {
        self.base += by;
        if by >= WINDOW {
            self.bits.iter_mut().for_each(|w| *w = 0);
            return;
        }
        let words = (by / 64) as usize;
        let shift = by % 64;
        let len = self.bits.len();
        for i in 0..len {
            let lo = self.bits.get(i + words).copied().unwrap_or(0);
            let hi = self.bits.get(i + words + 1).copied().unwrap_or(0);
            // `shift == 0` is special-cased because `hi << 64` is undefined.
            self.bits[i] = if shift == 0 {
                lo
            } else {
                (lo >> shift) | (hi << (64 - shift))
            };
        }
    }

    /// Advance `hw` over the run of set bits starting at `base`, if the window still sits
    /// directly on the water.
    ///
    /// Guarded on `base == hw + 1`: once a slide has opened a gap, the positions between
    /// the water and the window were never accepted, so no run of set bits above them may
    /// be folded into the prefix.
    fn absorb_contiguous_run(&mut self) {
        if self.base != self.hw + 1 {
            return;
        }
        let mut run = 0;
        while run < WINDOW && self.get(run) {
            run += 1;
        }
        if run > 0 {
            self.hw += run;
            self.slide(run);
        }
    }
}

/// What the gate decided about one envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not seen before. Spool it.
    Fresh,
    /// At or below the high water, or already in the window. Count it and drop it.
    Duplicate,
    /// Accepted, and the window had to slide so far to hold it that `skipped` positions
    /// below it are no longer deduplicated here.
    ///
    /// Not an error, and **not** a change to the high water: the skipped positions fall
    /// back to the database backstop (W7 §4.4's stated cost), and a later envelope in that
    /// range is answered `Fresh` rather than being dropped. An operator watching
    /// `mw_ingest_window_overrun_total` move is watching a peer that is either replaying an
    /// enormous backlog or sending sequence numbers it invented; the per-envelope `seq`
    /// sanity bound that would refuse the second case is §4.6's, which is M6-4's seam.
    FreshAfterOverrun {
        /// How many positions dropped out of the window's reach.
        skipped: u64,
    },
    /// The origin table is at [`super::MAX_ORIGINS`], so this origin could not be interned
    /// and the envelope is dropped (W7 **D-8**).
    ///
    /// A verdict rather than a separate call, because interning and recording the high
    /// water have to happen in one critical section: two lock acquisitions could interleave
    /// with a third origin's and leave a `hw_seq` recorded against a row that is not there.
    OriginTableFull,
}

/// Every origin's dedup state, keyed by `NodeId`.
///
/// Keyed by `NodeId` rather than by the manifest's dense `origin_id` on purpose: the
/// in-memory structure is rebuilt from the newest segment at every start (§3.6 step 5) and
/// never written, so it has no reason to carry the persisted encoding's index — and keying
/// it by the thing the wire carries removes one translation from the accept path.
#[derive(Debug, Default)]
pub struct Dedup {
    by_origin: BTreeMap<OriginId, Window>,
}

impl Dedup {
    /// An empty gate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed one origin's high water from the manifest, at open.
    pub fn seed(&mut self, origin: OriginId, hw: u64) {
        self.by_origin.insert(origin, Window::new(hw));
    }

    /// One origin's high water — what the manifest persists.
    pub fn high_water(&self, origin: OriginId) -> u64 {
        self.by_origin.get(&origin).map_or(0, |w| w.hw)
    }

    /// Whether this `(origin, seq)` has been seen, without recording it.
    ///
    /// `false` for anything the window cannot speak about, which is the direction that
    /// never loses an envelope.
    pub fn is_duplicate(&self, origin: OriginId, seq: u64) -> bool {
        let Some(w) = self.by_origin.get(&origin) else {
            return false;
        };
        if seq <= w.hw {
            return true;
        }
        if seq < w.base || seq >= w.base + WINDOW {
            return false;
        }
        w.get(seq - w.base)
    }

    /// What [`Dedup::accept`] would answer, without recording anything.
    ///
    /// The read half of the gate. `write_batch` needs it because the ceiling check and the
    /// `write_all` both have to happen before any state changes — so the verdict is taken
    /// first and committed only once the records are in the file.
    pub fn would_accept(&self, origin: OriginId, seq: u64) -> Verdict {
        if seq == 0 {
            return Verdict::Duplicate;
        }
        let Some(w) = self.by_origin.get(&origin) else {
            return Verdict::Fresh;
        };
        if seq <= w.hw {
            return Verdict::Duplicate;
        }
        if seq < w.base {
            return Verdict::Fresh;
        }
        if seq >= w.base + WINDOW {
            return Verdict::FreshAfterOverrun {
                skipped: seq - (w.base + WINDOW) + 1,
            };
        }
        if w.get(seq - w.base) {
            Verdict::Duplicate
        } else {
            Verdict::Fresh
        }
    }

    /// Record this `(origin, seq)` and say what it was.
    ///
    /// `seq == 0` is never fresh: [`mw_types::Seq::FIRST`] is 1, so a zero is either an
    /// encoder that forgot the field or a default-constructed message, and accepting it
    /// would put a record at a position the high water cannot distinguish from "nothing
    /// accepted yet".
    pub fn accept(&mut self, origin: OriginId, seq: u64) -> Verdict {
        if seq == 0 {
            return Verdict::Duplicate;
        }
        let w = self
            .by_origin
            .entry(origin)
            .or_insert_with(|| Window::new(0));
        if seq <= w.hw {
            return Verdict::Duplicate;
        }
        if seq < w.base {
            // Below the window but above the water: a position a previous slide gave up
            // on. Unknowable, so accepted -- see the type's doc for why that direction.
            return Verdict::Fresh;
        }
        let mut skipped = 0;
        if seq >= w.base + WINDOW {
            // Slide so `seq` is the last slot, which keeps the most recent history
            // deduplicated. `hw` deliberately does not move.
            skipped = seq - (w.base + WINDOW) + 1;
            w.slide(skipped);
        }
        let offset = seq - w.base;
        if w.get(offset) {
            return Verdict::Duplicate;
        }
        w.set(offset);
        w.absorb_contiguous_run();
        if skipped > 0 {
            Verdict::FreshAfterOverrun { skipped }
        } else {
            Verdict::Fresh
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> OriginId {
        OriginId::new([n; 16])
    }

    #[test]
    fn an_in_order_stream_advances_the_high_water_and_leaves_the_window_empty() {
        let mut d = Dedup::new();
        for seq in 1..=1_000 {
            assert_eq!(d.accept(node(1), seq), Verdict::Fresh, "seq {seq}");
        }
        assert_eq!(d.high_water(node(1)), 1_000);
    }

    #[test]
    fn a_replayed_prefix_is_every_envelope_a_duplicate() {
        let mut d = Dedup::new();
        for seq in 1..=100 {
            d.accept(node(1), seq);
        }
        for seq in 1..=100 {
            assert_eq!(d.accept(node(1), seq), Verdict::Duplicate, "seq {seq}");
        }
        assert_eq!(d.high_water(node(1)), 100, "and the water did not move");
    }

    /// W7 §2.3's diamond: `A:4712` arrives via B before `A:4711` arrives via C. The water
    /// must not be pinned, and neither envelope may be dropped.
    #[test]
    fn out_of_order_arrival_is_ordinary_and_the_water_catches_up() {
        let mut d = Dedup::new();
        for seq in 1..=4_710 {
            d.accept(node(1), seq);
        }
        assert_eq!(
            d.accept(node(1), 4_712),
            Verdict::Fresh,
            "the long way round"
        );
        assert_eq!(
            d.high_water(node(1)),
            4_710,
            "4712 is in the window, not the water"
        );
        assert_eq!(d.accept(node(1), 4_711), Verdict::Fresh, "the short way");
        assert_eq!(
            d.high_water(node(1)),
            4_712,
            "and the water advances over both"
        );
        assert_eq!(d.accept(node(1), 4_712), Verdict::Duplicate);
        assert_eq!(d.accept(node(1), 4_711), Verdict::Duplicate);
    }

    #[test]
    fn two_origins_do_not_see_each_others_sequences() {
        let mut d = Dedup::new();
        assert_eq!(d.accept(node(1), 500), Verdict::Fresh);
        assert_eq!(
            d.accept(node(2), 500),
            Verdict::Fresh,
            "node B's 500 is unrelated to node A's"
        );
        assert!(d.is_duplicate(node(1), 500));
        assert!(!d.is_duplicate(node(3), 500));
    }

    #[test]
    fn a_seeded_high_water_rejects_everything_at_or_below_it() {
        let mut d = Dedup::new();
        d.seed(node(1), 4_711);
        assert!(d.is_duplicate(node(1), 1));
        assert!(d.is_duplicate(node(1), 4_711));
        assert!(!d.is_duplicate(node(1), 4_712));
        assert_eq!(d.accept(node(1), 4_711), Verdict::Duplicate);
        assert_eq!(d.accept(node(1), 4_712), Verdict::Fresh);
    }

    /// A sequence number so far ahead that the window cannot hold it is accepted, the
    /// narrowing is reported, and **the high water does not move** — the positions it slid
    /// past were never accepted.
    #[test]
    fn a_jump_past_the_window_slides_it_without_moving_the_water() {
        let mut d = Dedup::new();
        d.accept(node(1), 1);
        let far = 1 + WINDOW + 10;
        assert_eq!(
            d.accept(node(1), far),
            Verdict::FreshAfterOverrun { skipped: 10 }
        );
        assert_eq!(
            d.high_water(node(1)),
            1,
            "the water still covers only what was accepted"
        );
        assert!(d.is_duplicate(node(1), far), "and `far` is remembered");
        assert!(
            !d.is_duplicate(node(1), 11),
            "a position the slide gave up on is answered Fresh, not Duplicate"
        );
    }

    /// **The regression test for the defect this module was rewritten for.** One envelope
    /// with a peer-chosen `seq` at the top of the range must not be able to refuse that
    /// origin's real traffic — which is what advancing the water on a slide did, for ever,
    /// because the water is persisted.
    #[test]
    fn a_hostile_sequence_number_cannot_wedge_an_origin() {
        let mut d = Dedup::new();
        assert!(matches!(
            d.accept(node(7), u64::MAX),
            Verdict::FreshAfterOverrun { .. }
        ));
        assert_eq!(
            d.high_water(node(7)),
            0,
            "nothing was accepted below it, so the water is still at zero"
        );
        for seq in 1..=5 {
            assert_eq!(
                d.accept(node(7), seq),
                Verdict::Fresh,
                "the origin's real traffic is still admissible at seq {seq}"
            );
        }
    }

    /// W7 §2.3's diamond on a *fresh* receiver: the first sight of A is a live envelope
    /// far above zero, and C's later backfill of earlier history must still be accepted.
    #[test]
    fn a_backfill_below_a_slid_window_is_accepted() {
        let mut d = Dedup::new();
        assert!(matches!(
            d.accept(node(1), 50_000),
            Verdict::FreshAfterOverrun { .. }
        ));
        assert_eq!(
            d.accept(node(1), 40_000),
            Verdict::Fresh,
            "the backfill is below the window, so it is accepted rather than dropped"
        );
    }

    /// The invariant the type's doc states, checked exhaustively over an adversarial mix:
    /// every `seq` at or below the high water must have been accepted at some point.
    #[test]
    fn the_high_water_only_ever_covers_accepted_sequence_numbers() {
        use std::collections::BTreeSet;

        let mut d = Dedup::new();
        let mut ever_accepted: BTreeSet<u64> = BTreeSet::new();
        // A deterministic but awkward order: in-order runs, backward gap-fillers, and two
        // jumps far past the window.
        let mut seqs: Vec<u64> = (1..=200).collect();
        seqs.extend([10_000, 9_000, 10_001, 1, 150, 20_000, 19_999]);
        seqs.extend((201..=400).rev());
        for seq in seqs {
            if d.accept(node(1), seq) != Verdict::Duplicate {
                ever_accepted.insert(seq);
            }
            let hw = d.high_water(node(1));
            for covered in 1..=hw {
                assert!(
                    ever_accepted.contains(&covered),
                    "the water reached {hw}, which covers {covered} -- never accepted"
                );
            }
        }
    }

    #[test]
    fn a_zero_sequence_number_is_never_fresh() {
        let mut d = Dedup::new();
        assert_eq!(d.accept(node(1), 0), Verdict::Duplicate);
        assert_eq!(d.high_water(node(1)), 0);
    }

    /// The bitmap slide is the fiddly part, so it gets its own check: a sparse set of
    /// accepted sequence numbers survives an advance that is not a multiple of 64.
    #[test]
    fn the_window_survives_a_shift_that_is_not_a_multiple_of_sixty_four() {
        let mut d = Dedup::new();
        let sparse = [100u64, 101, 103, 200, 4_000];
        for seq in sparse {
            assert_eq!(d.accept(node(1), seq), Verdict::Fresh, "seq {seq}");
        }
        // Fill 1..=99, which advances the water to 101 (100 and 101 were already set).
        for seq in 1..=99 {
            assert_eq!(d.accept(node(1), seq), Verdict::Fresh, "seq {seq}");
        }
        assert_eq!(d.high_water(node(1)), 101);
        for seq in sparse {
            assert!(d.is_duplicate(node(1), seq), "seq {seq} after the shift");
        }
        assert!(!d.is_duplicate(node(1), 102));
        assert!(!d.is_duplicate(node(1), 199));
    }
}
