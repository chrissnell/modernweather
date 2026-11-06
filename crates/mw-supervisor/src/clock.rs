//! One clock for the whole process (**D-25**, W3 §3.3).
//!
//! **One `tokio::time::Sleep`, re-armed from a binary heap**, not N `Interval`s. The Go
//! tree hand-rolls 21 scheduling clocks plus an unused generic `RunPeriodicTask` helper
//! with zero callers — somebody had already reached this conclusion and had nowhere to put
//! it. What moving them here buys, beyond the deduplication: `next_run` is a heap peek,
//! `last_run` and `last_error` are table fields, and a configuration change rebuilds the
//! heap in one place.
//!
//! This module and [`crate::task`] are the only two permitted to call the scheduling
//! primitives `clippy.toml` disallows.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use mw_component::{
    stable_hash, ComponentId, CronSpec, FirstRun, LocalTime, OccurrenceKey, Phase, Trigger, Zone,
};
use tokio::time::Instant;

/// Which occurrence of a component's schedule is due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Occurrence {
    /// A firing of a `Trigger::Every`.
    Every {
        /// A monotonic tag, incremented per firing. **Not** a count of elapsed periods: a
        /// process stalled past a period advances to the next future slot rather than
        /// firing the backlog, so this counts firings and the slot it lands on is the
        /// clock's business. It exists so that two queued firings of one component are
        /// distinguishable for deduplication.
        seq: u64,
    },
    /// A `Trigger::Calendar` occurrence, keyed on local date and slot — which is what
    /// makes fall-back's second pass recognisable as already completed (W2a Decision 13).
    Calendar(OccurrenceKey),
    /// The single `Trigger::OnStartup` run.
    Startup,
    /// A `ComponentControl::run_now`, or a delivery on the reading stream, which has no
    /// schedule and therefore no occurrence of its own.
    Demand,
    /// Not an occurrence: the instant a component in `Backoff` is built again. It lives in
    /// the same heap because there is **one** clock for the whole process, and a second
    /// timer for restarts would be the twenty-second hand-rolled one.
    Restart,
}

impl Occurrence {
    /// The persisted key, for the occurrences that have one.
    pub fn key(&self) -> Option<&OccurrenceKey> {
        match self {
            Occurrence::Calendar(key) => Some(key),
            _ => None,
        }
    }
}

/// One scheduled firing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    /// When, on the monotonic clock.
    pub at: Instant,
    /// Whose.
    pub id: ComponentId,
    /// Which occurrence.
    pub occurrence: Occurrence,
}

// Ordered by instant, then by id, so that two components due in the same instant fire in
// a deterministic order rather than in whatever order the heap happens to hold them.
impl Ord for Due {
    fn cmp(&self, other: &Self) -> Ordering {
        self.at.cmp(&other.at).then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for Due {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The process's single timer.
#[derive(Debug)]
pub struct Clock {
    heap: BinaryHeap<Reverse<Due>>,
    /// The wall/monotonic reference, for detecting a stepped clock.
    wall_ref: (SystemTime, Instant),
    /// How many times the wall clock was observed jumping.
    pub clock_step_total: u64,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    /// An empty clock.
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            wall_ref: (SystemTime::now(), Instant::now()),
            clock_step_total: 0,
        }
    }

    /// When the next firing is due, for the one `sleep_until` the supervisor awaits.
    pub fn peek(&self) -> Option<Instant> {
        self.heap.peek().map(|Reverse(due)| due.at)
    }

    /// Every firing that is due at `now`, oldest first.
    ///
    /// More than one can be due at once after a `sleep` that overshot or a stepped clock,
    /// and a loop that took only the first would then fire the rest a period late.
    pub fn take_due(&mut self, now: Instant) -> Vec<Due> {
        let mut out = Vec::new();
        while let Some(Reverse(due)) = self.heap.peek() {
            if due.at > now {
                break;
            }
            let Some(Reverse(due)) = self.heap.pop() else {
                break;
            };
            out.push(due);
        }
        out
    }

    /// Schedule something immediately — `OnStartup` when its needs are first satisfied, or
    /// `run_now`.
    ///
    /// Deduplicated on `(id, occurrence)`, so a double-click in the UI runs once.
    pub fn push_now(&mut self, id: ComponentId, occurrence: Occurrence) {
        self.push(Due {
            at: Instant::now(),
            id,
            occurrence,
        });
    }

    /// Schedule one firing, unless the same `(id, occurrence)` is already queued.
    pub fn push(&mut self, due: Due) {
        if self
            .heap
            .iter()
            .any(|Reverse(d)| d.id == due.id && d.occurrence == due.occurrence)
        {
            return;
        }
        self.heap.push(Reverse(due));
    }

    /// Drop every firing for one component — what a stop or a plan diff does.
    pub fn forget(&mut self, id: &ComponentId) {
        self.heap.retain(|Reverse(due)| due.id != *id);
    }

    /// When this component's next firing is due, which is what `next_run` is — a heap
    /// peek rather than a guess.
    pub fn next_for(&self, id: &ComponentId) -> Option<Instant> {
        self.heap
            .iter()
            .filter(|Reverse(due)| due.id == *id)
            .map(|Reverse(due)| due.at)
            .min()
    }

    /// How many firings are queued. Diagnostic, and the assertion a test makes.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether anything is scheduled.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Arm a component's first firing from its trigger.
    pub fn arm(&mut self, id: &ComponentId, trigger: &Trigger, now_wall: DateTime<Utc>) {
        match trigger {
            Trigger::Every {
                period,
                first,
                phase,
                ..
            } => {
                let (at, seq) = match first {
                    FirstRun::Immediate => (Instant::now(), 0),
                    FirstRun::AtPhase => (next_slot(id, *period, *phase, now_wall), 1),
                };
                self.push(Due {
                    at,
                    id: id.clone(),
                    occurrence: Occurrence::Every { seq },
                });
            }
            Trigger::Calendar { spec, tz, .. } => {
                if let Some(zone) = tz.zone() {
                    self.arm_calendar(id, spec, zone, now_wall);
                }
            }
            Trigger::OnStartup { .. } => self.push_now(id.clone(), Occurrence::Startup),
            Trigger::OnReading { .. } | Trigger::OnDemand { .. } | Trigger::Service => {}
        }
    }

    /// Arm the next `Calendar` occurrence strictly after `after`.
    pub fn arm_calendar(
        &mut self,
        id: &ComponentId,
        spec: &CronSpec,
        zone: Zone,
        after: DateTime<Utc>,
    ) {
        let Some(occurrence) = next_calendar(spec, zone, after) else {
            return;
        };
        self.push(Due {
            at: Self::instant_for(occurrence.at),
            id: id.clone(),
            occurrence: Occurrence::Calendar(OccurrenceKey::new(
                id.clone(),
                occurrence.local_date,
                occurrence.slot,
            )),
        });
    }

    /// Re-arm the firing after one that has just fired.
    ///
    /// Called **before** the outcome is interpreted (W3 §3.4), so a panic or an unexpected
    /// branch in the outcome handling cannot silently stop a component from ever running
    /// again. The Go `GetLoopPackets` defect W1 found — the Davis driver loops only on
    /// *error* and returns on success, so polling stops after the first successful batch
    /// of 20 — is this class of bug, and it survived because nothing else owned the
    /// schedule.
    pub fn rearm_after(&mut self, id: &ComponentId, trigger: &Trigger, fired: &Due) {
        match (trigger, &fired.occurrence) {
            (Trigger::Every { period, .. }, Occurrence::Every { seq }) => {
                // `fired.at + period`, on the monotonic clock: the phase grid established
                // when the component was armed is preserved exactly, with no drift from
                // however long the occurrence took.
                let mut at = fired.at + *period;
                let now = Instant::now();
                // If the process was stalled for longer than a period, advance to the
                // next future slot rather than firing the backlog: `Skip` is the only
                // overlap policy, and a queue that later fires as a burst is the thing
                // the rate limits exist to prevent.
                while at <= now {
                    at += *period;
                }
                self.push(Due {
                    at,
                    id: id.clone(),
                    occurrence: Occurrence::Every { seq: seq + 1 },
                });
            }
            (Trigger::Every { .. }, _) => {
                // A `run_now` on a periodic component does not disturb its schedule: the
                // scheduled occurrence is still in the heap.
            }
            (Trigger::Calendar { spec, tz, .. }, _) => {
                if let Some(zone) = tz.zone() {
                    // From the occurrence that just fired, not from `now`: an occurrence
                    // that ran late must not cause the next one to be computed from the
                    // late instant and skip a slot.
                    let from = fired
                        .occurrence
                        .key()
                        .and_then(|key| resolve_key(spec, zone, key))
                        .unwrap_or_else(Utc::now);
                    self.arm_calendar(id, spec, zone, from);
                }
            }
            _ => {}
        }
    }

    /// Whether the wall clock has been stepped since the last check, and by how much.
    ///
    /// Each check compares the elapsed `SystemTime` against the elapsed `Instant`; a
    /// divergence over a second means the wall clock was corrected, which is the common
    /// case on the smallest release target — a Raspberry Pi boots with no RTC and NTP
    /// steps it minutes or years. The `Calendar` half of the heap is then recomputed from
    /// scratch; `Every` entries are untouched, because they are monotonic.
    ///
    /// The Go version does not handle this at all.
    pub fn wall_step(&mut self) -> Option<Duration> {
        let (ref_wall, ref_mono) = self.wall_ref;
        let now_wall = SystemTime::now();
        let now_mono = Instant::now();
        let mono_elapsed = now_mono.duration_since(ref_mono);
        let wall_elapsed = now_wall
            .duration_since(ref_wall)
            .unwrap_or_else(|e| e.duration());
        self.wall_ref = (now_wall, now_mono);
        let divergence = wall_elapsed.abs_diff(mono_elapsed);
        if divergence > Duration::from_secs(1) {
            self.clock_step_total += 1;
            Some(divergence)
        } else {
            None
        }
    }

    /// Drop every queued `Calendar` firing, so they can be recomputed after a clock step.
    pub fn forget_calendar(&mut self) {
        self.heap
            .retain(|Reverse(due)| !matches!(due.occurrence, Occurrence::Calendar(_)));
    }

    /// Convert a wall-clock instant into a monotonic one.
    ///
    /// A `Calendar` occurrence is computed in a time zone and therefore in wall-clock
    /// terms; the heap is monotonic. This is the one conversion between them, and it is
    /// why a stepped clock invalidates exactly the calendar entries.
    ///
    /// Deliberately **not** through [`Clock::wall_step`]'s stored reference: that pair is
    /// as old as the last step check, and a conversion wants the offset *now*. Taking a
    /// fresh pair means a step that happened since the last check is absorbed here rather
    /// than baked into the instant.
    pub fn instant_for(wall: DateTime<Utc>) -> Instant {
        let now = Utc::now();
        let delta = wall.signed_duration_since(now);
        match delta.to_std() {
            Ok(d) => Instant::now() + d,
            // Already in the past: due now.
            Err(_) => Instant::now(),
        }
    }
}

/// The first firing of a periodic schedule: the next phase-aligned slot after `now`.
///
/// `Phase::Spread` is the default and the offset is derived from the component id (W2a
/// §1.5), so two Weather Underground uploaders for two stations are separated by a stable,
/// reproducible amount, "next run at" is answerable, and a restart does not reshuffle the
/// schedule. Today every uploader does "initial upload in 15 seconds" and then fires on
/// intervals aligned to that instant, so all of them hit the local database and their
/// upstream services together — and do it again in lockstep after every restart.
///
/// The slot grid is anchored to the Unix epoch rather than to process start, which is what
/// `Phase::Aligned` needs: `window:rollup` reads the 1-minute aggregate, and that
/// materialises on the minute whatever time the daemon happened to start.
fn next_slot(id: &ComponentId, period: Duration, phase: Phase, now_wall: DateTime<Utc>) -> Instant {
    let period_ns = period.as_nanos().max(1);
    let offset_ns = match phase {
        Phase::Spread => u128::from(stable_hash(id.as_str().as_bytes())) % period_ns,
        Phase::Aligned => 0,
        Phase::Fixed(d) => d.as_nanos() % period_ns,
    };
    let since_epoch =
        u128::try_from(now_wall.timestamp_nanos_opt().unwrap_or(0).max(0)).unwrap_or(0);
    let slots = since_epoch.saturating_sub(offset_ns) / period_ns;
    let next_slot_ns = (slots + 1) * period_ns + offset_ns;
    let ahead =
        Duration::from_nanos(u64::try_from(next_slot_ns.saturating_sub(since_epoch)).unwrap_or(0));
    // Converted to `Instant` once, here. From this point the schedule advances by whole
    // periods on the monotonic clock, which is what makes it immune to a stepped wall
    // clock — the property `Calendar` cannot have and does not claim.
    Instant::now() + ahead
}

/// One resolved calendar occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalendarOccurrence {
    /// When it fires.
    pub at: DateTime<Utc>,
    /// Its local date, which is half the occurrence key.
    pub local_date: NaiveDate,
    /// Its slot, which is the other half.
    pub slot: u32,
}

/// The instant an already-known occurrence key fires at, so the next one can be computed
/// from the occurrence rather than from the clock.
fn resolve_key(spec: &CronSpec, zone: Zone, key: &OccurrenceKey) -> Option<DateTime<Utc>> {
    slots_on(spec, key.local_date)
        .into_iter()
        .find(|(slot, _)| *slot == key.slot)
        .and_then(|(_, time)| resolve(zone, key.local_date, time))
}

/// How many days forward or back the search looks. A yearly schedule needs a full year
/// plus a leap day; 400 covers both without an unbounded loop.
const SEARCH_DAYS: i64 = 400;

/// The first occurrence strictly after `after`.
pub fn next_calendar(
    spec: &CronSpec,
    zone: Zone,
    after: DateTime<Utc>,
) -> Option<CalendarOccurrence> {
    let start = after.with_timezone(&zone.tz()).date_naive();
    for day in 0..=SEARCH_DAYS {
        let date = start.checked_add_signed(chrono::Duration::days(day))?;
        for (slot, time) in slots_on(spec, date) {
            if let Some(at) = resolve(zone, date, time) {
                if at > after {
                    return Some(CalendarOccurrence {
                        at,
                        local_date: date,
                        slot,
                    });
                }
            }
        }
    }
    None
}

/// The last occurrence at or before `before` — what misfire handling needs.
pub fn previous_calendar(
    spec: &CronSpec,
    zone: Zone,
    before: DateTime<Utc>,
) -> Option<CalendarOccurrence> {
    let start = before.with_timezone(&zone.tz()).date_naive();
    for day in 0..=SEARCH_DAYS {
        let date = start.checked_sub_signed(chrono::Duration::days(day))?;
        let mut best: Option<CalendarOccurrence> = None;
        for (slot, time) in slots_on(spec, date) {
            if let Some(at) = resolve(zone, date, time) {
                if at <= before {
                    best = Some(CalendarOccurrence {
                        at,
                        local_date: date,
                        slot,
                    });
                }
            }
        }
        if best.is_some() {
            return best;
        }
    }
    None
}

/// Which slots a spec declares on one local date.
fn slots_on(spec: &CronSpec, date: NaiveDate) -> Vec<(u32, LocalTime)> {
    match spec {
        CronSpec::Daily { slots } => slots
            .iter()
            .enumerate()
            .map(|(i, t)| (u32::try_from(i).unwrap_or(u32::MAX), *t))
            .collect(),
        CronSpec::Monthly { day, at } => {
            if date.day() == clamp_day(*day, date) {
                vec![(0, *at)]
            } else {
                Vec::new()
            }
        }
        CronSpec::Yearly { month, day, at } => {
            if date.month() == u32::from(*month) && date.day() == clamp_day(*day, date) {
                vec![(0, *at)]
            } else {
                Vec::new()
            }
        }
    }
}

/// A declared day-of-month, clamped to the length of the month it falls in.
///
/// The 31st in February fires on the 28th or 29th, because a monthly rollover that
/// silently skips February is worse than one that happens a day early — the same reasoning
/// W2a §1.6 gives for firing a nonexistent local time after the DST gap rather than
/// skipping it.
fn clamp_day(day: u8, date: NaiveDate) -> u32 {
    let last = days_in_month(date.year(), date.month());
    u32::from(day).min(last)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first = NaiveDate::from_ymd_opt(year, month, 1);
    let next_first = NaiveDate::from_ymd_opt(next_year, next_month, 1);
    match (first, next_first) {
        (Some(a), Some(b)) => u32::try_from((b - a).num_days()).unwrap_or(28),
        _ => 28,
    }
}

/// Resolve a local date and time in `zone` to an instant.
///
/// Delegates to [`Zone::at`], which carries the DST rules and their reasoning. It was
/// written here and moved down a layer when `mw_pipeline`'s rollup needed the same
/// arithmetic for a station's local day, month and year boundaries: the two crates are in
/// the same layer and may not name each other, so a copy in each would be a copy that can
/// disagree about which instant a rollover happens at.
///
/// Kept as a named function rather than inlined at its three call sites so that this
/// module's own tests — the ones that assert W2a Decision 13 against `America/Denver` —
/// keep testing the thing the clock actually calls.
pub fn resolve(zone: Zone, date: NaiveDate, time: LocalTime) -> Option<DateTime<Utc>> {
    zone.at(date, time)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mw_component::{CronSpec, LocalTime};

    fn zone() -> Zone {
        Zone::new("America/Denver").unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse::<DateTime<Utc>>().unwrap()
    }

    fn daily(hour: u8, minute: u8) -> CronSpec {
        CronSpec::daily_at(LocalTime::new(hour, minute))
    }

    #[test]
    fn a_daily_occurrence_is_the_next_local_time_in_the_zone() {
        // 03:00 America/Denver on 2026-11-10 is 10:00 UTC (MST, UTC-7).
        let next = next_calendar(&daily(3, 0), zone(), at("2026-11-10T04:00:00Z")).unwrap();
        assert_eq!(next.at, at("2026-11-10T10:00:00Z"));
        assert_eq!(next.local_date.to_string(), "2026-11-10");
        assert_eq!(next.slot, 0);

        // Just after it fires, the next one is the following day.
        let after = next_calendar(&daily(3, 0), zone(), next.at).unwrap();
        assert_eq!(after.at, at("2026-11-11T10:00:00Z"));
    }

    /// W2a Decision 13, first half. 2027-03-14 02:30 does not exist in America/Denver: the
    /// clock jumps from 02:00 to 03:00. The occurrence fires at 03:00 local rather than
    /// being skipped.
    #[test]
    fn a_nonexistent_local_time_fires_after_the_gap() {
        let next = next_calendar(&daily(2, 30), zone(), at("2027-03-14T00:00:00Z")).unwrap();
        assert_eq!(next.at, at("2027-03-14T09:00:00Z"));
        assert_eq!(next.local_date.to_string(), "2027-03-14");
        // 09:00Z is 03:00 MDT, the first instant that exists after the gap.
        assert_eq!(
            next.at
                .with_timezone(&zone().tz())
                .format("%H:%M")
                .to_string(),
            "03:00"
        );
    }

    /// W2a Decision 13, second half. 2026-11-01 01:30 happens twice in America/Denver.
    /// The first pass is the one that fires, and the occurrence key is the same for both,
    /// so the second is recognised as already completed rather than double-counting a
    /// rollover.
    #[test]
    fn an_ambiguous_local_time_fires_on_the_first_pass_only() {
        let first = next_calendar(&daily(1, 30), zone(), at("2026-11-01T00:00:00Z")).unwrap();
        // 01:30 MDT is 07:30Z; the second pass, 01:30 MST, would be 08:30Z.
        assert_eq!(first.at, at("2026-11-01T07:30:00Z"));

        // Asking again from just after the first pass skips the second and goes to the
        // next day, because the slot for 2026-11-01 is behind us.
        let next = next_calendar(&daily(1, 30), zone(), first.at).unwrap();
        assert_eq!(next.local_date.to_string(), "2026-11-02");
    }

    /// **W2a Decision 13 at the slot that matters: local midnight.** The existing two
    /// tests use 02:30 and 01:30 in `America/Denver`, where midnight is ordinary — so a
    /// daily rollover scheduled at 00:00 was never exercised against a transition that
    /// happens *at* 00:00. Two zones have one every year:
    ///
    /// - `America/Havana` goes 00:00 -> 01:00 in March (midnight does not exist) and
    ///   01:00 -> 00:00 in November (midnight happens twice).
    /// - `America/Santiago` goes 00:00 -> 01:00 in September.
    ///
    /// The property asserted is `implementation-plan.md` M3-4's done-criterion from the
    /// clock's side: walking a year of occurrences produces **exactly one per local date**,
    /// each with a distinct occurrence key, and the keys are the local dates. The component
    /// half — that one occurrence moves the boundary once — is
    /// `mw_pipeline::rollup::state`'s.
    #[test]
    fn a_daily_midnight_occurrence_is_one_per_local_date_across_a_midnight_transition() {
        for name in [
            "America/Havana",
            "America/Santiago",
            "America/Denver",
            // The control: Arizona observes no DST, so every local day is 24 hours and the
            // walk must still produce one occurrence per date.
            "America/Phoenix",
        ] {
            let z = Zone::new(name).unwrap_or_else(|e| panic!("{name}: {e}"));
            let spec = daily(0, 0);
            let mut cursor = at("2026-01-01T00:00:00Z");
            let mut dates: Vec<NaiveDate> = Vec::new();
            let mut instants: Vec<DateTime<Utc>> = Vec::new();
            // Four hundred occurrences: more than a year, so both of a zone's transitions
            // are inside the walk whichever months they fall in.
            for _ in 0..400 {
                let Some(next) = next_calendar(&spec, z, cursor) else {
                    panic!("{name}: no occurrence after {cursor}");
                };
                assert!(next.at > cursor, "{name}: {next:?} is not after {cursor}");
                assert_eq!(next.slot, 0, "{name}: one slot a day");
                dates.push(next.local_date);
                instants.push(next.at);
                cursor = next.at;
            }

            // One occurrence per local date, in order, with no date repeated and none
            // skipped. A repeat is fall-back firing twice; a skip is spring-forward
            // swallowing a day's rollover, which is the invisible failure that corrupts
            // one day's totals once a year.
            let unique: std::collections::BTreeSet<NaiveDate> = dates.iter().copied().collect();
            assert_eq!(unique.len(), dates.len(), "{name}: a local date repeated");
            for pair in dates.windows(2) {
                assert_eq!(
                    pair[1],
                    pair[0]
                        .succ_opt()
                        .unwrap_or_else(|| panic!("{name}: {:?} has no next day", pair[0])),
                    "{name}: {:?} is not the day after {:?}",
                    pair[1],
                    pair[0]
                );
            }

            // The occurrence key is the local date, so the second pass through an
            // ambiguous midnight produces a key the supervisor has already recorded --
            // which is what `handle_misfire` and `Clock::push_now` deduplicate on.
            let id = ComponentId::new("rain:rollover:backyard").expect("a valid id");
            let keys: std::collections::BTreeSet<OccurrenceKey> = dates
                .iter()
                .map(|d| OccurrenceKey::new(id.clone(), *d, 0))
                .collect();
            assert_eq!(keys.len(), dates.len(), "{name}: two dates, one key");

            // And the days really are 23, 24 or 25 hours long, which is what a boundary
            // computed as "the previous one plus 86,400 seconds" would get wrong.
            let lengths: std::collections::BTreeSet<i64> = instants
                .windows(2)
                .map(|w| (w[1] - w[0]).num_hours())
                .collect();
            assert!(
                lengths.iter().all(|h| (23..=25).contains(h)),
                "{name}: day lengths {lengths:?}"
            );
            if name == "America/Phoenix" {
                assert_eq!(
                    lengths,
                    std::collections::BTreeSet::from([24]),
                    "Arizona observes no DST, and it still has a local midnight a \
                     rollover resets on"
                );
            } else {
                assert!(
                    lengths.contains(&23) && lengths.contains(&25),
                    "{name} transitions in each direction within the walk, so it must \
                     contain a 23-hour and a 25-hour day: {lengths:?}"
                );
            }
        }
    }

    #[test]
    fn several_daily_slots_keep_their_index() {
        let spec = CronSpec::Daily {
            slots: vec![
                LocalTime::new(0, 10),
                LocalTime::new(3, 0),
                LocalTime::new(23, 45),
            ],
        };
        // 09:00Z is 02:00 MST on the 10th, so the next slot of that local day is 03:00.
        let first = next_calendar(&spec, zone(), at("2026-11-10T09:00:00Z")).unwrap();
        assert_eq!(first.slot, 1, "03:00 local is slot 1");
        assert_eq!(first.local_date.to_string(), "2026-11-10");
        let second = next_calendar(&spec, zone(), first.at).unwrap();
        assert_eq!(second.slot, 2);
        let third = next_calendar(&spec, zone(), second.at).unwrap();
        assert_eq!(third.slot, 0);
        assert_eq!(third.local_date.to_string(), "2026-11-11");
    }

    #[test]
    fn a_monthly_day_clamps_to_the_length_of_the_month() {
        let spec = CronSpec::Monthly {
            day: 31,
            at: LocalTime::new(0, 10),
        };
        let feb = next_calendar(&spec, zone(), at("2027-02-01T00:00:00Z")).unwrap();
        assert_eq!(feb.local_date.to_string(), "2027-02-28");
        let leap = next_calendar(&spec, zone(), at("2028-02-01T00:00:00Z")).unwrap();
        assert_eq!(leap.local_date.to_string(), "2028-02-29");
    }

    #[test]
    fn a_yearly_occurrence_is_found_within_the_search_window() {
        let spec = CronSpec::Yearly {
            month: 1,
            day: 1,
            at: LocalTime::MIDNIGHT,
        };
        let next = next_calendar(&spec, zone(), at("2026-11-10T00:00:00Z")).unwrap();
        assert_eq!(next.local_date.to_string(), "2027-01-01");
        assert_eq!(next.at, at("2027-01-01T07:00:00Z"));
    }

    #[test]
    fn the_previous_occurrence_is_what_misfire_handling_asks_for() {
        let prev = previous_calendar(&daily(3, 0), zone(), at("2026-11-10T04:00:00Z")).unwrap();
        assert_eq!(prev.at, at("2026-11-09T10:00:00Z"));
        assert_eq!(prev.local_date.to_string(), "2026-11-09");
    }

    #[test]
    fn the_heap_fires_in_instant_order_and_breaks_ties_by_id() {
        let mut clock = Clock::new();
        let now = Instant::now();
        for (id, secs) in [("c:third", 30), ("c:first", 10), ("c:second", 20)] {
            clock.push(Due {
                at: now + Duration::from_secs(secs),
                id: ComponentId::new(id).unwrap(),
                occurrence: Occurrence::Every { seq: 1 },
            });
        }
        // Two at the same instant: the ids order them.
        clock.push(Due {
            at: now + Duration::from_secs(10),
            id: ComponentId::new("c:aardvark").unwrap(),
            occurrence: Occurrence::Every { seq: 1 },
        });
        assert_eq!(clock.len(), 4);
        let due = clock.take_due(now + Duration::from_secs(25));
        let fired: Vec<_> = due.iter().map(|d| d.id.as_str().to_string()).collect();
        assert_eq!(fired, vec!["c:aardvark", "c:first", "c:second"]);
        assert_eq!(clock.len(), 1);
    }

    #[test]
    fn a_duplicate_occurrence_is_not_queued_twice() {
        let mut clock = Clock::new();
        let id = ComponentId::new("check:aprs-is").unwrap();
        clock.push_now(id.clone(), Occurrence::Demand);
        clock.push_now(id.clone(), Occurrence::Demand);
        assert_eq!(clock.len(), 1, "a double-click runs once");
        clock.forget(&id);
        assert!(clock.is_empty());
    }

    /// `Phase::Spread` must be stable across restarts, and two components must differ.
    #[test]
    fn the_spread_phase_is_stable_and_separates_two_components() {
        let period = Duration::from_secs(60);
        let now = at("2026-11-10T04:00:00Z");
        let a = ComponentId::new("wu:backyard").unwrap();
        let b = ComponentId::new("wu:frontyard").unwrap();
        let offset =
            |id: &ComponentId| u128::from(stable_hash(id.as_str().as_bytes())) % period.as_nanos();
        assert_ne!(offset(&a), offset(&b));

        // Armed twice, the same component lands the same distance into the period.
        let mut clock = Clock::new();
        let trigger = Trigger::every(period, Duration::from_secs(30));
        clock.arm(&a, &trigger, now);
        let first = clock.peek().unwrap();
        clock.forget(&a);
        clock.arm(&a, &trigger, now);
        let second = clock.peek().unwrap();
        let drift = first.max(second) - first.min(second);
        assert!(
            drift < Duration::from_millis(50),
            "the phase moved by {drift:?} between two arms of the same id"
        );
    }

    #[test]
    fn an_immediate_first_run_is_due_now() {
        let mut clock = Clock::new();
        let id = ComponentId::new("window:rollup").unwrap();
        clock.arm(
            &id,
            &Trigger::Every {
                period: Duration::from_secs(60),
                deadline: Duration::from_secs(10),
                first: FirstRun::Immediate,
                phase: Phase::Aligned,
                floor: Duration::ZERO,
            },
            at("2026-11-10T04:00:00Z"),
        );
        let due = clock.take_due(Instant::now());
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].occurrence, Occurrence::Every { seq: 0 });
    }

    #[test]
    fn calendar_entries_can_be_dropped_without_touching_periodic_ones() {
        let mut clock = Clock::new();
        let now = at("2026-11-10T04:00:00Z");
        let periodic = ComponentId::new("wu:backyard").unwrap();
        let rollover = ComponentId::new("snow:season").unwrap();
        clock.arm(
            &periodic,
            &Trigger::every(Duration::from_secs(60), Duration::from_secs(30)),
            now,
        );
        clock.arm_calendar(&rollover, &daily(0, 10), zone(), now);
        assert_eq!(clock.len(), 2);
        clock.forget_calendar();
        assert_eq!(clock.len(), 1);
    }
}
