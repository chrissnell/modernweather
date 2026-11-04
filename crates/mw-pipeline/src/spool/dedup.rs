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
/// `hw` is the contiguous prefix: everything at or below it has been accepted. `above` is
/// a bitmap of the `WINDOW` sequence numbers after it, so `hw + 1 + i` is set iff bit `i`
/// is set. When `hw` advances, the bitmap shifts.
#[derive(Debug, Clone)]
struct Window {
    hw: u64,
    /// `WINDOW` bits. `hw + 1` is bit 0.
    above: Vec<u64>,
}

impl Window {
    fn new(hw: u64) -> Self {
        Self {
            hw,
            above: vec![0; (WINDOW / 64) as usize],
        }
    }

    fn get(&self, offset: u64) -> bool {
        let (word, bit) = ((offset / 64) as usize, offset % 64);
        self.above[word] & (1 << bit) != 0
    }

    fn set(&mut self, offset: u64) {
        let (word, bit) = ((offset / 64) as usize, offset % 64);
        self.above[word] |= 1 << bit;
    }

    /// Shift the bitmap down by `by` positions, which is what advancing `hw` means.
    fn shift(&mut self, by: u64) {
        if by >= WINDOW {
            self.above.iter_mut().for_each(|w| *w = 0);
            return;
        }
        let words = (by / 64) as usize;
        let bits = by % 64;
        let len = self.above.len();
        for i in 0..len {
            let lo = self.above.get(i + words).copied().unwrap_or(0);
            let hi = self.above.get(i + words + 1).copied().unwrap_or(0);
            self.above[i] = if bits == 0 {
                lo
            } else {
                (lo >> bits) | (hi << (64 - bits))
            };
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
    /// Far enough above the high water that the window cannot hold it, so the window was
    /// advanced and `skipped` sequence numbers are no longer deduplicated.
    ///
    /// Not an error: the envelope is accepted. It is reported separately because it is the
    /// one case where the gate's guarantee narrows to the database backstop, and an
    /// operator watching `mw_ingest_window_overrun_total` move is watching a peer that is
    /// either replaying an enormous backlog or sending nonsense sequence numbers.
    FreshAfterOverrun {
        /// How many sequence numbers fell out of the window.
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
    pub fn is_duplicate(&self, origin: OriginId, seq: u64) -> bool {
        let Some(w) = self.by_origin.get(&origin) else {
            return false;
        };
        if seq <= w.hw {
            return true;
        }
        let offset = seq - w.hw - 1;
        offset < WINDOW && w.get(offset)
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
        let mut overrun = 0;
        let mut offset = seq - w.hw - 1;
        if offset >= WINDOW {
            // The window cannot reach this far. Advance it so that `seq` is the last slot,
            // which is the choice that keeps the most recent history deduplicated.
            let by = offset - WINDOW + 1;
            w.shift(by);
            w.hw += by;
            overrun = by;
            offset = WINDOW - 1;
        }
        if w.get(offset) {
            return Verdict::Duplicate;
        }
        w.set(offset);
        // Advance the high water over the contiguous run the new bit may have completed,
        // which is what keeps the window from filling up in the ordinary in-order case.
        let mut advance = 0;
        while advance < WINDOW && w.get(advance) {
            advance += 1;
        }
        if advance > 0 {
            w.shift(advance);
            w.hw += advance;
        }
        if overrun > 0 {
            Verdict::FreshAfterOverrun { skipped: overrun }
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

    /// A sequence number so far ahead that the window cannot hold it is still accepted,
    /// and the narrowing is reported rather than silent.
    #[test]
    fn a_jump_past_the_window_is_accepted_and_counted() {
        let mut d = Dedup::new();
        d.accept(node(1), 1);
        let far = 1 + WINDOW + 10;
        assert_eq!(
            d.accept(node(1), far),
            Verdict::FreshAfterOverrun { skipped: 10 }
        );
        assert_eq!(
            d.high_water(node(1)),
            11,
            "the water advanced just far enough that `far` is the window's last slot"
        );
        assert!(d.is_duplicate(node(1), far), "and it is remembered");
        assert!(
            !d.is_duplicate(node(1), 12),
            "and the positions the window gave up on are no longer deduplicated here"
        );
    }

    #[test]
    fn a_zero_sequence_number_is_never_fresh() {
        let mut d = Dedup::new();
        assert_eq!(d.accept(node(1), 0), Verdict::Duplicate);
        assert_eq!(d.high_water(node(1)), 0);
    }

    /// The bitmap shift is the fiddly part, so it gets its own property: a sparse set of
    /// accepted sequence numbers survives an arbitrary advance of the water.
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
