//! [`Trigger`] — when a component runs. W2a §1.4, §1.5 and §1.6 as code.
//!
//! Six variants, and the supervisor owns the clock for every one of them (**D-25**). The
//! Go tree hand-rolls 21 scheduling clocks, each with its own error handling and none of
//! them inspectable; the point of moving them here is not the deduplication but that "when
//! did this last run, when does it run next, did it fail" becomes state (W2a §1.3).

use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

use mw_types::{OriginId, StationId, StationKey};

/// When a component runs.
///
/// Every scheduled variant carries a `deadline`, and `deadline <= period` is checked at
/// plan time — which is what makes `Skip` the only overlap policy this system needs (W2a
/// Decision 8). A tick that exceeds its deadline is cancelled and counted; a skip becomes
/// a diagnostic rather than a routine occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// Driven by reading arrival. The supervisor delivers batches; the component holds no
    /// receiver (W2a §5).
    OnReading {
        /// Which readings this component wants.
        filter: ReadingFilter,
        /// What may be lost, and how.
        policy: ReadingPolicy,
        /// The per-batch deadline.
        deadline: Duration,
    },

    /// Fixed period.
    Every {
        /// The interval between occurrences.
        period: Duration,
        /// The per-occurrence deadline. `<= period`, enforced at plan time.
        deadline: Duration,
        /// Whether the first occurrence is immediate or at the next phase slot.
        first: FirstRun,
        /// How this component's occurrences are offset from everyone else's.
        phase: Phase,
        /// The hard minimum this component's period may be (**W2a Decision 56**).
        ///
        /// Declared by the registrar, because only the component's own crate knows that
        /// Weather Underground's floor is 60 s. Enforced **once**, in `plan()`, rather
        /// than clamped and logged on every call as `ResolveUploadInterval` does today.
        /// `Duration::ZERO` for a component with no floor.
        floor: Duration,
    },

    /// Wall-clock schedule in a named time zone.
    Calendar {
        /// What times, locally.
        spec: CronSpec,
        /// Whose time zone.
        tz: TzSource,
        /// What to do about an occurrence the process was down for.
        misfire: Misfire,
        /// The per-occurrence deadline.
        deadline: Duration,
    },

    /// Once, when its requirements are first satisfied. Terminal state `Finished`.
    OnStartup {
        /// The deadline for that one run.
        deadline: Duration,
    },

    /// Only when asked, through `ComponentControl`. No schedule.
    OnDemand {
        /// The deadline for a requested run.
        deadline: Duration,
    },

    /// Owns its own loop. No schedule, no deadline, no overlap policy.
    ///
    /// A `Service` owns time *inside* its loop — a read deadline, a reconnect backoff, a
    /// keep-alive — and that is not a D-25 violation, because a `Service` has no schedule
    /// to own (W2a Decision 7).
    Service,
}

impl Trigger {
    /// The deadline, for every variant that has one.
    pub fn deadline(&self) -> Option<Duration> {
        match self {
            Trigger::OnReading { deadline, .. }
            | Trigger::Every { deadline, .. }
            | Trigger::Calendar { deadline, .. }
            | Trigger::OnStartup { deadline }
            | Trigger::OnDemand { deadline } => Some(*deadline),
            Trigger::Service => None,
        }
    }

    /// Which executable trait this trigger requires. Total and statically checked — the
    /// table in W2a §1.2.
    pub fn body_kind(&self) -> BodyKind {
        match self {
            Trigger::OnReading { .. } => BodyKind::OnReading,
            Trigger::Every { .. }
            | Trigger::Calendar { .. }
            | Trigger::OnStartup { .. }
            | Trigger::OnDemand { .. } => BodyKind::Tick,
            Trigger::Service => BodyKind::Serve,
        }
    }

    /// Whether the supervisor's timer heap schedules this trigger.
    pub fn is_scheduled(&self) -> bool {
        matches!(self, Trigger::Every { .. } | Trigger::Calendar { .. })
    }

    /// W5's `schedule.kind`, which the management API renders.
    pub fn kind(&self) -> &'static str {
        match self {
            Trigger::OnReading { .. } => "on_reading",
            Trigger::Every { .. } => "every",
            Trigger::Calendar { .. } => "calendar",
            Trigger::OnStartup { .. } => "on_startup",
            Trigger::OnDemand { .. } => "on_demand",
            Trigger::Service => "service",
        }
    }

    /// `Every` with the defaults W2a §1.5 argues for: spread phase, first run at the next
    /// phase slot, and no floor.
    pub fn every(period: Duration, deadline: Duration) -> Self {
        Trigger::Every {
            period,
            deadline,
            first: FirstRun::AtPhase,
            phase: Phase::Spread,
            floor: Duration::ZERO,
        }
    }
}

/// Which of the three executable traits a trigger maps onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// [`crate::Tick`] — bounded work, nothing supplied.
    Tick,
    /// [`crate::OnReading`] — bounded work over a supplied batch.
    OnReading,
    /// [`crate::Serve`] — unbounded, owns its own loop.
    Serve,
}

/// How a component's occurrences are offset from every other component's.
///
/// Not random jitter. Random jitter destroys the predictability of "next run at", which is
/// one of the three things moving the clocks into the supervisor exists to provide, and it
/// buys nothing phase separation does not already buy (W2a Decision 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// `offset = stable_hash(component_id) mod period`. Deterministic across restarts, so
    /// two uploaders for two stations are separated by a reproducible amount and a restart
    /// does not reshuffle the schedule. The default.
    Spread,
    /// `offset = 0`. For a component whose input materialises on a wall-clock boundary —
    /// `window:rollup` reads the 1-minute aggregate, and a spread phase would read a
    /// bucket that is not there yet.
    Aligned,
    /// A fixed offset, for the rare case that needs one.
    Fixed(Duration),
}

/// When the first occurrence happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstRun {
    /// At the next phase-aligned slot — within one period. The default, and what replaces
    /// the hard-coded 15-second initial upload (W2a Decision 10).
    AtPhase,
    /// Immediately on reaching `Running`, for warm-up work.
    Immediate,
}

/// A validated IANA time zone.
///
/// The parse is the validation (W2a Decision 11): `"MST"` and `"-0700"` are rejected,
/// because a fixed offset cannot express DST and DST is the entire reason `Calendar`
/// exists. Holding the resolved [`chrono_tz::Tz`] rather than the name is what lets the
/// supervisor compute an occurrence without reading configuration — it must not, because
/// `ConfigStore` is a capability it does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone(chrono_tz::Tz);

impl Zone {
    /// Resolve an IANA name against the compiled zone database.
    ///
    /// Three spellings are rejected even though `chrono_tz` parses them, and the
    /// distinction is deliberate: what W2a Decision 11 rejects is a name that denotes an
    /// **offset** rather than a **place**.
    ///
    /// - `"MST"`, `"EST"` — a zone abbreviation. It is one fixed offset, so a station in it
    ///   cannot have a DST transition at all.
    /// - `"EST5EDT"`, `"Etc/GMT+7"`, `"-0700"` — the same thing with a rule or a number
    ///   bolted on.
    ///
    /// `"America/Phoenix"` is accepted, and that is the case that shows why the rule is
    /// about the name and not about whether the zone's offset ever changes: Arizona
    /// observes no DST, and a station there still has a local midnight that a rollover
    /// must reset on.
    pub fn new(name: &str) -> Result<Self, ZoneError> {
        let is_area_location = name.contains('/') && !name.starts_with("Etc/");
        let acceptable = (is_area_location || name == "UTC")
            && !name.contains(|c: char| c.is_ascii_digit() || c == '+' || c == '-');
        if !acceptable {
            return Err(ZoneError::NotAZone {
                name: name.to_string(),
            });
        }
        name.parse::<chrono_tz::Tz>()
            .map(Zone)
            .map_err(|_| ZoneError::NotAZone {
                name: name.to_string(),
            })
    }

    /// The resolved zone, for the occurrence arithmetic.
    pub fn tz(self) -> chrono_tz::Tz {
        self.0
    }

    /// The IANA name, for a log field and for W5.
    pub fn name(self) -> &'static str {
        self.0.name()
    }
}

impl fmt::Display for Zone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a declared time zone is not usable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ZoneError {
    /// Not an IANA zone name, or a fixed-offset alias that cannot express DST.
    #[error("`{name}` is not an IANA time zone that can express daylight saving")]
    NotAZone {
        /// What was declared.
        name: String,
    },
}

/// Whose time zone a [`Trigger::Calendar`] is expressed in.
///
/// The zone is `None` when the thing it was taken from declares none, or declares one that
/// is not usable — the registrar resolves it and `plan()` rejects the absence, so a
/// station with a `Calendar` component and no valid zone is a plan error rather than a
/// silent fallback to UTC. That fallback is exactly what the Go rollovers do today:
/// `date_trunc('day', NOW())` truncates in the database server's zone and the device's
/// declared `timezone` is used nowhere but rendering sun times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TzSource {
    /// The station's zone.
    Station {
        /// Whose zone it is.
        station: StationId,
        /// The resolved zone, if the station has a usable one.
        zone: Option<Zone>,
    },
    /// The deployment's configured default zone, for a component not scoped to a station.
    Deployment {
        /// The resolved zone, if the deployment configures a usable one.
        zone: Option<Zone>,
    },
}

impl TzSource {
    /// The resolved zone, or `None` if there is none to resolve.
    pub fn zone(&self) -> Option<Zone> {
        match self {
            TzSource::Station { zone, .. } | TzSource::Deployment { zone } => *zone,
        }
    }

    /// Which station's zone this is, for the plan error's message.
    pub fn station(&self) -> Option<StationId> {
        match self {
            TzSource::Station { station, .. } => Some(*station),
            TzSource::Deployment { .. } => None,
        }
    }
}

/// A local time of day, in a [`TzSource`]'s zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalTime {
    /// 0–23.
    pub hour: u8,
    /// 0–59.
    pub minute: u8,
}

impl LocalTime {
    /// Midnight.
    pub const MIDNIGHT: Self = Self { hour: 0, minute: 0 };

    /// A local time of day. Validated by `plan()`, not here, so that an invalid
    /// configuration produces one `PlanError` with the component named rather than a
    /// panic in a constructor.
    pub const fn new(hour: u8, minute: u8) -> Self {
        Self { hour, minute }
    }

    /// Whether this is a real time of day.
    pub const fn is_valid(&self) -> bool {
        self.hour < 24 && self.minute < 60
    }
}

impl fmt::Display for LocalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

/// A wall-clock schedule.
///
/// Deliberately **not** a cron string. Every `Calendar` user in this system is a daily
/// rollover, a daily sweep or a yearly boundary (W2a §4.3), so a five-field cron
/// vocabulary would be a parser, an ambiguity (`day-of-month` *and* `day-of-week`) and a
/// surface area nothing asks for. The slot index in an occurrence key is a position in
/// [`CronSpec::Daily::slots`], which only stays meaningful because this is a list rather
/// than an expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronSpec {
    /// Every day, at each of these local times. Strictly increasing; the slot index is the
    /// position in this list.
    Daily {
        /// The local times, in order.
        slots: Vec<LocalTime>,
    },
    /// Monthly, on `day` at `at`. A `day` beyond the length of a month fires on that
    /// month's last day, because a monthly rollover that silently skips February is worse
    /// than one that happens a day early.
    Monthly {
        /// 1–31.
        day: u8,
        /// The local time.
        at: LocalTime,
    },
    /// Yearly, on `month`/`day` at `at`.
    Yearly {
        /// 1–12.
        month: u8,
        /// 1–31.
        day: u8,
        /// The local time.
        at: LocalTime,
    },
}

impl CronSpec {
    /// Daily at one local time.
    pub fn daily_at(at: LocalTime) -> Self {
        CronSpec::Daily { slots: vec![at] }
    }

    /// How many slots a day has, which bounds the occurrence key's slot index.
    pub fn slots_per_day(&self) -> usize {
        match self {
            CronSpec::Daily { slots } => slots.len(),
            CronSpec::Monthly { .. } | CronSpec::Yearly { .. } => 1,
        }
    }

    /// Why this spec cannot be scheduled, if it cannot.
    ///
    /// Returned rather than panicking, so `plan()` reports it against the component that
    /// declared it.
    pub fn problem(&self) -> Option<CronProblem> {
        match self {
            CronSpec::Daily { slots } => {
                if slots.is_empty() {
                    return Some(CronProblem::NoSlots);
                }
                if let Some(bad) = slots.iter().find(|s| !s.is_valid()) {
                    return Some(CronProblem::BadTime(*bad));
                }
                if slots.windows(2).any(|w| w[0] >= w[1]) {
                    return Some(CronProblem::SlotsOutOfOrder);
                }
                None
            }
            CronSpec::Monthly { day, at } => {
                if !at.is_valid() {
                    Some(CronProblem::BadTime(*at))
                } else if !(1..=31).contains(day) {
                    Some(CronProblem::BadDay(*day))
                } else {
                    None
                }
            }
            CronSpec::Yearly { month, day, at } => {
                if !at.is_valid() {
                    Some(CronProblem::BadTime(*at))
                } else if !(1..=12).contains(month) {
                    Some(CronProblem::BadMonth(*month))
                } else if !(1..=31).contains(day) {
                    Some(CronProblem::BadDay(*day))
                } else {
                    None
                }
            }
        }
    }
}

/// Why a [`CronSpec`] cannot be scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronProblem {
    /// A daily spec with no times in it.
    NoSlots,
    /// A daily spec whose times are not strictly increasing, so a slot index would not
    /// identify an occurrence.
    SlotsOutOfOrder,
    /// An hour or minute out of range.
    BadTime(LocalTime),
    /// A day outside 1–31.
    BadDay(u8),
    /// A month outside 1–12.
    BadMonth(u8),
}

impl fmt::Display for CronProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CronProblem::NoSlots => f.write_str("no times are declared"),
            CronProblem::SlotsOutOfOrder => {
                f.write_str("the declared times are not strictly increasing")
            }
            CronProblem::BadTime(t) => {
                write!(f, "{:02}:{:02} is not a time of day", t.hour, t.minute)
            }
            CronProblem::BadDay(d) => write!(f, "day {d} is not in 1..=31"),
            CronProblem::BadMonth(m) => write!(f, "month {m} is not in 1..=12"),
        }
    }
}

/// What to do about an occurrence the process was down for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misfire {
    /// Run the most recent missed occurrence only, if it is within `grace`. The default,
    /// because every `Calendar` user in this system is idempotent and
    /// only-the-latest-matters: a rollover recomputes a total from stored data, a sweep
    /// deletes what has expired. Running all twelve missed occurrences after a twelve-hour
    /// outage would do eleven units of useless work.
    RunLatest {
        /// How stale a missed occurrence may be and still be made up.
        grace: Duration,
    },
    /// Never make up a missed occurrence; record it and wait for the next. Nothing in the
    /// current system needs this; it exists so that something which must not be made up
    /// has somewhere to say so.
    Skip,
}

impl Misfire {
    /// `RunLatest { grace: 24h }`, the default of W2a Decision 14.
    pub const DEFAULT: Self = Misfire::RunLatest {
        grace: Duration::from_secs(24 * 60 * 60),
    };
}

/// Which readings a [`Trigger::OnReading`] component wants.
///
/// Declared at the subscription rather than decided inside a send path. Today APRS filters
/// by calling `station.Capabilities().Has(Weather)` inside `sendStationReading`, and the
/// gRPC receiver's `Capabilities()` carries a `TODO` saying it cannot report what its
/// remote stations actually measure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadingFilter {
    /// Everything.
    All,
    /// Only these stations.
    Stations(StationSet),
    /// Everything except what this origin has already seen — loop prevention for
    /// multi-hop topologies. W7 §2 owns the semantics.
    NotFrom(OriginId),
}

/// A set of stations, for [`ReadingFilter::Stations`].
///
/// `BTreeSet` rather than `HashSet`: the sets are small, and an ordered set makes a spec
/// fingerprint stable, which the plan diff depends on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StationSet(BTreeSet<StationKey>);

impl StationSet {
    /// An empty set, which matches nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a station.
    pub fn insert(&mut self, station: StationKey) {
        self.0.insert(station);
    }

    /// Whether a station is in the set.
    pub fn contains(&self, station: &StationKey) -> bool {
        self.0.contains(station)
    }

    /// The stations, in order.
    pub fn iter(&self) -> impl Iterator<Item = &StationKey> {
        self.0.iter()
    }

    /// How many stations are in the set.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<StationKey> for StationSet {
    fn from_iter<T: IntoIterator<Item = StationKey>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// What may be lost on the way to a [`Trigger::OnReading`] component, and how.
///
/// The invariant this serves is **a source is never blocked by a sink** (W2a Decision 61).
/// The current violation is severe: the reading distributor performs a blocking, unguarded
/// send per engine into a 20-deep channel, so a stalled TimescaleDB back-pressures the
/// serial read loops and a Davis console that is not drained loses packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadingPolicy {
    /// Must not lose data — the storage writer, the collector uplink.
    ///
    /// Because the stream never blocks, `Durable` **cannot** be satisfied by an in-memory
    /// channel: a bounded queue means loss and an unbounded one means an OOM on a stalled
    /// database. It requires a local durable spool at the fan-out point (W2a Decision 62),
    /// which is implementation-plan.md M3-2's deliverable.
    Durable,
    /// Only the newest matters — the snapshot publisher, the live stream. A coalescing
    /// watch, never blocking.
    Latest,
    /// Lossy is acceptable; overflow is counted as `lagged_total`.
    Lossy,
}

impl ReadingPolicy {
    /// The label value for `mw_readings_dropped_total` and for W5's rendering.
    pub const fn as_str(self) -> &'static str {
        match self {
            ReadingPolicy::Durable => "durable",
            ReadingPolicy::Latest => "latest",
            ReadingPolicy::Lossy => "lossy",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_trigger_maps_onto_exactly_one_executable_trait() {
        let d = Duration::from_secs(1);
        assert_eq!(
            Trigger::every(Duration::from_secs(60), d).body_kind(),
            BodyKind::Tick
        );
        assert_eq!(
            Trigger::OnStartup { deadline: d }.body_kind(),
            BodyKind::Tick
        );
        assert_eq!(
            Trigger::OnDemand { deadline: d }.body_kind(),
            BodyKind::Tick
        );
        assert_eq!(
            Trigger::Calendar {
                spec: CronSpec::daily_at(LocalTime::new(3, 0)),
                tz: TzSource::Deployment {
                    zone: Some(Zone::new("America/Denver").unwrap()),
                },
                misfire: Misfire::DEFAULT,
                deadline: d,
            }
            .body_kind(),
            BodyKind::Tick
        );
        assert_eq!(
            Trigger::OnReading {
                filter: ReadingFilter::All,
                policy: ReadingPolicy::Durable,
                deadline: d,
            }
            .body_kind(),
            BodyKind::OnReading
        );
        assert_eq!(Trigger::Service.body_kind(), BodyKind::Serve);
        assert_eq!(Trigger::Service.deadline(), None);
    }

    #[test]
    fn a_fixed_offset_is_not_a_zone() {
        assert!(Zone::new("America/Denver").is_ok());
        assert_eq!(
            Zone::new("America/Denver").unwrap().name(),
            "America/Denver"
        );
        // Arizona observes no DST and is still a place with a local midnight.
        assert!(Zone::new("America/Phoenix").is_ok());
        assert!(Zone::new("UTC").is_ok());
        for rejected in [
            "MST",
            "EST",
            "-0700",
            "Etc/GMT+7",
            "EST5EDT",
            "Mars/Olympus",
            "",
            "America/Denver2",
        ] {
            assert!(
                Zone::new(rejected).is_err(),
                "{rejected} should not be accepted"
            );
        }
    }

    #[test]
    fn a_cron_spec_reports_why_it_cannot_be_scheduled() {
        assert_eq!(
            CronSpec::Daily { slots: vec![] }.problem(),
            Some(CronProblem::NoSlots)
        );
        assert_eq!(
            CronSpec::Daily {
                slots: vec![LocalTime::new(3, 0), LocalTime::new(1, 0)]
            }
            .problem(),
            Some(CronProblem::SlotsOutOfOrder)
        );
        assert_eq!(
            CronSpec::Daily {
                slots: vec![LocalTime::new(24, 0)]
            }
            .problem(),
            Some(CronProblem::BadTime(LocalTime::new(24, 0)))
        );
        assert_eq!(
            CronSpec::Yearly {
                month: 13,
                day: 1,
                at: LocalTime::MIDNIGHT
            }
            .problem(),
            Some(CronProblem::BadMonth(13))
        );
        assert_eq!(
            CronSpec::Monthly {
                day: 0,
                at: LocalTime::MIDNIGHT
            }
            .problem(),
            Some(CronProblem::BadDay(0))
        );
        assert_eq!(CronSpec::daily_at(LocalTime::new(3, 0)).problem(), None);
        assert_eq!(CronSpec::daily_at(LocalTime::new(3, 0)).slots_per_day(), 1);
    }
}
