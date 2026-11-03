//! Repetition control (§5.6): the one hook with whole-pipeline reach.
//!
//! A disconnected serial port on a 1-second reconnect backoff emits 3,600 identical lines
//! an hour. That is the difference between logs that are useful and logs nobody reads, and
//! it is why this is a layer rather than a feature of each sink: `tracing_subscriber`'s
//! `Layered` **ANDs `event_enabled` across the stack**, so a layer returning `false` there
//! suppresses the event for *every* sink -- stderr, both rings, and anything added later.
//! No other hook has that reach.
//!
//! Three properties are load-bearing and each one is a test below:
//!
//! - **Keyed on the callsite, not the message.** `event.metadata().callsite()` is stable
//!   and free; a formatted message carrying a varying port name or address would never
//!   match itself. `station` is in the key so one failing device does not suppress
//!   another's identical error.
//! - **Nothing is delayed.** The first `burst` occurrences are emitted immediately, which
//!   is the property that makes suppression acceptable at all.
//! - **Suppression is itself visible.** While a key is suppressing, one summary per
//!   `summary_every` names how many were dropped and over what window.
//!
//! # Where the summary is emitted from, and why it is not emitted from here
//!
//! §5.6 writes the summary as a `warn!` from the suppression path. It cannot be: an event
//! macro goes through `tracing::dispatcher::get_default`, which refuses to re-enter a
//! dispatch that is already in progress on this thread and hands the event to
//! `Dispatch::none()` -- so a summary emitted from inside `event_enabled` reaches no sink
//! under a scoped subscriber, which is every test that would prove it works. So
//! [`Dedupe::pending`] accumulates and [`Dedupe::sweep`] emits, called from the binary's
//! own loop tick alongside the watchdog and the recorder's upkeep. The observable
//! behaviour §5.6 specifies is unchanged -- 3 immediate lines plus one summary a minute
//! instead of 3,600 lines an hour -- and `sweep_at` is a pure function of the clock, which
//! is what makes the arithmetic a unit test rather than a sixty-second test.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::callsite::Identifier;
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::record::Value;
use crate::span;

/// The target whose events are never suppressed, because an audit log with holes is not
/// an audit log: configuration mutations, authentication outcomes, certificate reloads.
pub const AUDIT_TARGET: &str = "mw::audit";

/// The target this layer's own summaries carry.
///
/// Exempt from suppression for a mechanical reason: every summary shares one callsite, so
/// a throttled summary stream would be one bucket for every key in the table, and the
/// second suppressed key of the minute would silence the first one's summary.
pub const DEDUPE_TARGET: &str = "mw::dedupe";

/// The field whose value marks an event that must never be suppressed.
///
/// `class = "fatal"` rather than a level: §5.6 exempts the *classification*, and
/// [`mw_types::Class`] is decided beside the error rather than at the call site. An event
/// that wants the exemption records `class = %e.class().as_str()`.
pub const CLASS_FIELD: &str = "class";

/// The field that keys one device's events separately from another's.
pub const STATION_FIELD: &str = "station";

/// §5.6's policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// How many occurrences of one key are emitted immediately.
    pub burst: u32,
    /// How long one token takes to come back.
    pub refill: Duration,
    /// How many keys are tracked before the least recently seen is evicted.
    pub keys: usize,
    /// How often a suppressing key reports what it suppressed.
    pub summary_every: Duration,
}

/// The shipped policy: burst 3, one token per 30 s, 1,024 keys, a summary a minute.
pub const DEDUPE: Policy = Policy {
    burst: 3,
    refill: Duration::from_secs(30),
    keys: 1024,
    summary_every: Duration::from_secs(60),
};

/// What identifies one repeating event (§5.6).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    callsite: Identifier,
    level: Level,
    station: Option<String>,
    class: Option<String>,
}

/// One key's token bucket and its suppression tally.
#[derive(Debug)]
struct Bucket {
    tokens: u32,
    /// Advanced by whole refill periods, so the remainder of a partial period is not lost
    /// to rounding on every event.
    refilled_at: Instant,
    last_seen: Instant,
    suppressed: u64,
    reported_at: Instant,
    /// Where the callsite is, for the summary's `callsite` field. Captured at insert
    /// because the summary is emitted later, from somewhere that has no event in hand.
    target: &'static str,
    line: Option<u32>,
}

impl Bucket {
    fn new(policy: &Policy, now: Instant, target: &'static str, line: Option<u32>) -> Self {
        Bucket {
            tokens: policy.burst,
            refilled_at: now,
            last_seen: now,
            suppressed: 0,
            reported_at: now,
            target,
            line,
        }
    }

    fn refill(&mut self, policy: &Policy, now: Instant) {
        let elapsed = now.saturating_duration_since(self.refilled_at);
        let periods = divide(elapsed, policy.refill);
        if periods == 0 {
            return;
        }
        self.tokens = self.tokens.saturating_add(periods).min(policy.burst);
        self.refilled_at += policy.refill * periods;
    }
}

/// How many whole `unit`s fit in `elapsed`, saturating at `u32::MAX`.
fn divide(elapsed: Duration, unit: Duration) -> u32 {
    let unit = unit.as_nanos();
    if unit == 0 {
        return u32::MAX;
    }
    u32::try_from(elapsed.as_nanos() / unit).unwrap_or(u32::MAX)
}

/// One suppression summary, ready to be emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// How many occurrences were dropped.
    pub suppressed: u64,
    /// Over how long, in seconds.
    pub window_s: u64,
    /// Where they came from, as `target:line`.
    pub callsite: String,
    /// The level they were emitted at, so the summary reads at the same severity.
    pub level: Level,
}

/// The suppression state, shared between the layer and the binary's sweep.
#[derive(Debug)]
pub struct Dedupe {
    policy: Policy,
    buckets: Mutex<HashMap<Key, Bucket>>,
}

impl Dedupe {
    /// A dedupe state under a policy.
    pub fn new(policy: Policy) -> Self {
        Dedupe {
            policy,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// The policy in force.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// How many keys are tracked.
    pub fn tracked(&self) -> usize {
        self.lock().len()
    }

    /// Whether an event with these key parts may be emitted, at this instant.
    ///
    /// Pure in the clock, which is what makes the token arithmetic a unit test rather
    /// than a thirty-second one.
    pub fn admit_at(
        &self,
        meta: &'static Metadata<'static>,
        station: Option<String>,
        class: Option<String>,
        now: Instant,
    ) -> bool {
        let target = meta.target();
        if never_suppressed(target, class.as_deref()) {
            return true;
        }
        let key = Key {
            callsite: meta.callsite(),
            level: *meta.level(),
            station,
            class,
        };
        let line = meta.line();
        let mut buckets = self.lock();
        if !buckets.contains_key(&key) && buckets.len() >= self.policy.keys {
            evict_oldest(&mut buckets);
        }
        let bucket = buckets
            .entry(key)
            .or_insert_with(|| Bucket::new(&self.policy, now, target, line));
        bucket.last_seen = now;
        bucket.refill(&self.policy, now);
        if bucket.tokens > 0 {
            bucket.tokens -= 1;
            true
        } else {
            bucket.suppressed += 1;
            false
        }
    }

    /// Every summary due at this instant, clearing the tallies it reports.
    pub fn sweep_at(&self, now: Instant) -> Vec<Summary> {
        let mut due = Vec::new();
        let mut buckets = self.lock();
        for (key, bucket) in buckets.iter_mut() {
            if bucket.suppressed == 0
                || now.saturating_duration_since(bucket.reported_at) < self.policy.summary_every
            {
                continue;
            }
            due.push(Summary {
                suppressed: bucket.suppressed,
                window_s: now.saturating_duration_since(bucket.reported_at).as_secs(),
                callsite: match bucket.line {
                    Some(line) => format!("{}:{line}", bucket.target),
                    None => bucket.target.to_string(),
                },
                level: key.level,
            });
            bucket.suppressed = 0;
            bucket.reported_at = now;
        }
        // Deterministic, because a `HashMap` iteration order is not: a test that asserts
        // two summaries should not depend on which bucket hashed first.
        due.sort_by(|a, b| {
            a.callsite
                .cmp(&b.callsite)
                .then(b.suppressed.cmp(&a.suppressed))
        });
        due
    }

    /// Emit every due summary. Called from the binary's loop tick, not from the
    /// suppression path -- see this module's documentation for why.
    pub fn sweep(&self) {
        for s in self.sweep_at(Instant::now()) {
            // One callsite per level, because `tracing`'s level is part of the static
            // metadata and the summary should read at the severity it is standing in for.
            // `target` is this module's exemption, which is what stops the summaries from
            // throttling each other.
            match s.level {
                Level::ERROR => tracing::error!(
                    target: DEDUPE_TARGET,
                    suppressed = s.suppressed,
                    window_s = s.window_s,
                    callsite = %s.callsite,
                    "repeated event suppressed"
                ),
                Level::WARN => tracing::warn!(
                    target: DEDUPE_TARGET,
                    suppressed = s.suppressed,
                    window_s = s.window_s,
                    callsite = %s.callsite,
                    "repeated event suppressed"
                ),
                _ => tracing::info!(
                    target: DEDUPE_TARGET,
                    suppressed = s.suppressed,
                    window_s = s.window_s,
                    callsite = %s.callsite,
                    "repeated event suppressed"
                ),
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Bucket>> {
        // Recovered rather than propagated: a poisoned dedupe table would mean no logging
        // at all, and every critical section here is map and integer arithmetic.
        self.buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// §5.6's exemptions, plus the two this stack's own shape forces.
///
/// - `mw::audit` and anything beneath it: an audit log with holes is not an audit log.
/// - `class = "fatal"`: §5.6's second exemption, keyed on the classification rather than
///   the level because that is where the decision is made (§4.2).
/// - `mw::dedupe`: every summary shares one callsite, so throttling them would mean the
///   second suppressed key of the minute silenced the first one's summary.
/// - `mw::access`: **the access stream is one callsite emitting thousands of distinct
///   records**, and `station` and `class` are the only things that split a key -- so a
///   token bucket over it would cap the access log at three lines per thirty seconds and
///   the 16,384-record ring would never fill. §5.7's sizing argument is explicitly that
///   an afternoon of normal traffic *does* overflow that ring, so the two sections only
///   agree if repetition control does not apply to this target. Suppressing a request log
///   is also the wrong thing on its own terms: two identical `GET /latest` lines are two
///   requests, not one event repeated.
fn never_suppressed(target: &str, class: Option<&str>) -> bool {
    target == AUDIT_TARGET
        || target.starts_with("mw::audit::")
        || target == DEDUPE_TARGET
        || target == crate::ring::ACCESS_TARGET
        || class == Some(mw_types::Class::Fatal.as_str())
}

/// Evict the least recently seen key, counted so that a table that is too small is
/// visible rather than merely slower.
fn evict_oldest(buckets: &mut HashMap<Key, Bucket>) {
    let victim = buckets
        .iter()
        .min_by_key(|(_, b)| b.last_seen)
        .map(|(k, _)| k.clone());
    if let Some(k) = victim {
        buckets.remove(&k);
        metrics::counter!("mw_dedupe_evicted_total").increment(1);
    }
}

/// The layer. Holds no state of its own: [`Dedupe`] is shared with the binary, which
/// sweeps it.
#[derive(Debug)]
pub struct DedupeLayer {
    state: std::sync::Arc<Dedupe>,
}

impl DedupeLayer {
    /// A layer over shared state.
    pub fn new(state: std::sync::Arc<Dedupe>) -> Self {
        DedupeLayer { state }
    }
}

impl<S> Layer<S> for DedupeLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        span::store(attrs, id, &ctx);
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        span::extend(id, values, &ctx);
    }

    fn event_enabled(&self, event: &Event<'_>, ctx: Context<'_, S>) -> bool {
        let (station, class) = key_parts(event, &ctx);
        self.state
            .admit_at(event.metadata(), station, class, Instant::now())
    }
}

/// `station` and `class` for the key: the event's own values if it recorded them,
/// otherwise the nearest enclosing span's.
///
/// The event's fields are visited only when its *static* metadata declares one of the two
/// names, so the ordinary event -- which declares neither -- costs two string comparisons
/// rather than an allocation.
fn key_parts<S>(event: &Event<'_>, ctx: &Context<'_, S>) -> (Option<String>, Option<String>)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let fields = event.metadata().fields();
    let declares = |name: &str| fields.field(name).is_some();
    let (mut station, mut class) = if declares(STATION_FIELD) || declares(CLASS_FIELD) {
        let own = crate::record::Fields::of(event);
        (
            own.get(STATION_FIELD).map(Value::to_string),
            own.get(CLASS_FIELD).map(Value::to_string),
        )
    } else {
        (None, None)
    };
    if station.is_none() {
        station = span::nearest(event, ctx, STATION_FIELD);
    }
    if class.is_none() {
        class = span::nearest(event, ctx, CLASS_FIELD);
    }
    (station, class)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::callsite::DefaultCallsite;
    use tracing::field::FieldSet;
    use tracing::metadata::Kind;

    /// Two callsites and an audit one, built the way the `event!` macro builds them.
    ///
    /// A real `Metadata` is needed rather than a stand-in because the key is the
    /// *callsite identity*: §5.6's whole argument is that the message is not stable and
    /// the callsite is, so a test that keyed on anything else would not be testing this.
    macro_rules! callsite {
        ($cs:ident, $meta:ident, $name:literal, $target:expr, $level:expr, $line:expr) => {
            static $meta: Metadata<'static> = Metadata::new(
                $name,
                $target,
                $level,
                Some(file!()),
                Some($line),
                Some(module_path!()),
                FieldSet::new(&["message"], tracing::callsite::Identifier(&$cs)),
                Kind::EVENT,
            );
            static $cs: DefaultCallsite = DefaultCallsite::new(&$meta);
        };
    }

    callsite!(
        SERIAL_CS,
        SERIAL,
        "event",
        "mw_station::serial",
        Level::WARN,
        214
    );
    callsite!(
        OTHER_CS,
        OTHER,
        "event",
        "mw_station::serial",
        Level::WARN,
        301
    );
    callsite!(AUDIT_CS, AUDIT, "event", AUDIT_TARGET, Level::INFO, 12);

    fn dedupe() -> Dedupe {
        Dedupe::new(DEDUPE)
    }

    fn admit(
        d: &Dedupe,
        meta: &'static Metadata<'static>,
        station: Option<&str>,
        at: Instant,
    ) -> bool {
        d.admit_at(meta, station.map(str::to_owned), None, at)
    }

    #[test]
    fn the_first_three_are_immediate_and_the_fourth_is_not() {
        let d = dedupe();
        let now = Instant::now();
        // Nothing is delayed: three at the same instant, all emitted.
        for i in 0..3 {
            assert!(
                admit(&d, &SERIAL, None, now),
                "occurrence {i} must be immediate"
            );
        }
        assert!(!admit(&d, &SERIAL, None, now), "the fourth is suppressed");
    }

    #[test]
    fn one_token_comes_back_every_thirty_seconds_and_the_bucket_does_not_overfill() {
        let d = dedupe();
        let t0 = Instant::now();
        for _ in 0..4 {
            admit(&d, &SERIAL, None, t0);
        }
        assert!(!admit(&d, &SERIAL, None, t0 + Duration::from_secs(29)));
        assert!(admit(&d, &SERIAL, None, t0 + Duration::from_secs(30)));
        assert!(!admit(&d, &SERIAL, None, t0 + Duration::from_secs(31)));
        // An hour of silence does not bank 120 tokens: the burst is the ceiling, so the
        // serial port that has been quiet all night still only gets three lines.
        let later = t0 + Duration::from_secs(3600);
        for _ in 0..DEDUPE.burst {
            assert!(admit(&d, &SERIAL, None, later));
        }
        assert!(!admit(&d, &SERIAL, None, later));
    }

    #[test]
    fn one_failing_device_does_not_suppress_another() {
        let d = dedupe();
        let now = Instant::now();
        for _ in 0..4 {
            admit(&d, &SERIAL, Some("backyard"), now);
        }
        assert!(
            admit(&d, &SERIAL, Some("roof"), now),
            "`station` is in the key, so roof has its own budget"
        );
    }

    #[test]
    fn two_callsites_in_one_module_are_two_keys() {
        let d = dedupe();
        let now = Instant::now();
        for _ in 0..4 {
            admit(&d, &SERIAL, None, now);
        }
        assert!(admit(&d, &OTHER, None, now));
    }

    #[test]
    fn the_audit_target_and_a_fatal_class_are_never_suppressed() {
        let d = dedupe();
        let now = Instant::now();
        for i in 0..50 {
            assert!(
                admit(&d, &AUDIT, None, now),
                "an audit log with holes is not an audit log (occurrence {i})"
            );
        }
        for i in 0..50 {
            assert!(
                d.admit_at(&SERIAL, None, Some("fatal".into()), now),
                "a fatal event is never suppressed (occurrence {i})"
            );
        }
        assert_eq!(d.tracked(), 0, "neither exemption allocates a bucket");
    }

    #[test]
    fn the_access_target_is_exempt_so_the_request_log_is_not_a_token_bucket() {
        callsite!(
            ACCESS_CS,
            ACCESS,
            "event",
            crate::ring::ACCESS_TARGET,
            Level::INFO,
            88
        );
        let d = dedupe();
        let now = Instant::now();
        for i in 0..5000 {
            assert!(
                admit(&d, &ACCESS, None, now),
                "request {i} is a request, not an event repeated"
            );
        }
    }

    #[test]
    fn a_summary_reports_what_was_suppressed_and_then_starts_again() {
        let d = dedupe();
        let t0 = Instant::now();
        for _ in 0..103 {
            admit(&d, &SERIAL, None, t0);
        }
        assert!(
            d.sweep_at(t0 + Duration::from_secs(59)).is_empty(),
            "the window has not closed yet"
        );
        let due = d.sweep_at(t0 + Duration::from_secs(60));
        assert_eq!(due.len(), 1, "{due:?}");
        assert_eq!(due[0].suppressed, 100, "103 occurrences, 3 of them emitted");
        assert_eq!(due[0].window_s, 60);
        assert_eq!(due[0].callsite, "mw_station::serial:214");
        assert_eq!(due[0].level, Level::WARN);
        assert!(
            d.sweep_at(t0 + Duration::from_secs(200)).is_empty(),
            "a reported tally is cleared, so the next summary counts only new ones"
        );
    }

    #[test]
    fn the_table_is_bounded_and_evicts_the_least_recently_seen() {
        let d = Dedupe::new(Policy { keys: 3, ..DEDUPE });
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        admit(&d, &SERIAL, Some("a"), at(0));
        for _ in 0..DEDUPE.burst {
            admit(&d, &SERIAL, Some("b"), at(1));
        }
        admit(&d, &SERIAL, Some("c"), at(2));
        assert_eq!(d.tracked(), 3, "the table is full");

        // A fourth key evicts `a`, which is the least recently seen, rather than the one
        // that is actually suppressing.
        admit(&d, &SERIAL, Some("d"), at(3));
        assert_eq!(d.tracked(), 3, "bounded at the policy's key count");
        assert!(
            !admit(&d, &SERIAL, Some("b"), at(4)),
            "`b` kept its spent bucket; the eviction took the idle key"
        );
        assert!(
            admit(&d, &SERIAL, Some("a"), at(5)),
            "an evicted key comes back with a full bucket"
        );
    }

    #[test]
    fn a_zero_refill_does_not_divide_by_zero() {
        let d = Dedupe::new(Policy {
            refill: Duration::ZERO,
            ..DEDUPE
        });
        let now = Instant::now();
        for _ in 0..10 {
            assert!(
                admit(&d, &SERIAL, None, now),
                "every token is back instantly"
            );
        }
    }
}
