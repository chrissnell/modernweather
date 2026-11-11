//! [`RadioInterval`] — the RF beacon floor as a type, which is W2a Decision 60.
//!
//! > *A `FrameSink::KissSerial` or `KissTcp` cannot be constructed with a faster interval,
//! > so a too-fast RF beacon is a config parse error and not a runtime warning.*
//! > — `docs/component-model.md` §4b.5
//!
//! # Why the radio is asymmetric with everything else
//!
//! An HTTP service answers `429` with `Retry-After` and APRS-IS can refuse a login. A radio
//! cannot: flooding a shared band has no feedback mechanism at all, and the only protection
//! is the local floor. So for the two KISS transports the floor is a constraint in the type
//! rather than a configuration value with a warning beside it, and the component carrying
//! one cannot be built at all if the interval is below it.
//!
//! Clamp-and-log stays proportionate for APRS-IS and for the HTTP uploaders, where a
//! feedback channel exists — `crate::plan` does that, once, at plan time, rather than per
//! send. `ResolveUploadInterval` in the Go tree logs a warning on **every call**, which for
//! a 60-second uploader is a warning a minute forever and leaves the configuration still
//! saying the wrong thing.
//!
//! # The floor is 600 s, and that was the owner's to decide
//!
//! **O-8**, answered **600 s** in `docs/implementation-plan.md`'s owner-input table:
//!
//! > | ~~M6-2, M7-4~~ | ~~**O-8** the RF beacon floor~~ | **Answered: 600 s.** M7-4's
//! > migration raises a sub-600 s KISS device with a one-line report |
//!
//! It is the one number in `docs/component-model.md` that changes existing behaviour:
//! today's `aprs_upload_interval` default is 300 s and applies to RF and internet alike.
//! The half that depends on the answer is **M7-4's** — migrating an existing sub-600 s KISS
//! device, which raises it and reports that in one line. The `uploader` table's
//! `interval_seconds` column deliberately carries no `CHECK` for this, and its own schema
//! comment says why: *"W2a OQ-A's upgrade path requires a sub-floor value to be RAISED with
//! a one-line report rather than rejected -- which a constraint here would make
//! impossible."* So the floor lives here and not in the schema, and what this type refuses
//! is a sub-floor value reaching a **transmitter**, not a sub-floor value existing in a
//! column M7-4 has to read.

use std::fmt;
use std::time::Duration;

/// An interval at or above the RF beacon floor.
///
/// There is no `From<Duration>`, no `Default` and no public field: [`RadioInterval::new`]
/// is the only constructor, which is what makes the floor a property of every value of this
/// type rather than a check somebody has to remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RadioInterval(Duration);

impl RadioInterval {
    /// The floor: **600 s**, the owner's answer to O-8.
    pub const FLOOR: Duration = Duration::from_secs(600);

    /// An interval, or the reason it cannot be one.
    pub fn new(interval: Duration) -> Result<Self, TooFast> {
        if interval < Self::FLOOR {
            return Err(TooFast {
                configured: interval,
                floor: Self::FLOOR,
            });
        }
        Ok(Self(interval))
    }

    /// The interval.
    pub fn get(self) -> Duration {
        self.0
    }
}

impl fmt::Display for RadioInterval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} s", self.0.as_secs())
    }
}

/// A configured RF beacon interval below the floor.
///
/// Its own type rather than a variant of a larger enum, because the one thing a caller can
/// do with it is decline to build the component — and a type whose only inhabitant means
/// "this transmitter would beacon too often" reads that way at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "a KISS beacon interval of {} s is below the {} s RF floor. A radio has no way to tell \
     us to slow down, so this is refused rather than clamped: raise \
     `uploader.interval_seconds` for this uploader, or leave it NULL for the default. \
     APRS-IS and the HTTP uploaders are clamped with a warning instead, because they can \
     push back.", configured.as_secs(), floor.as_secs()
)]
pub struct TooFast {
    /// What configuration asked for.
    configured: Duration,
    /// The floor.
    floor: Duration,
}

impl TooFast {
    /// What configuration asked for.
    pub fn configured(&self) -> Duration {
        self.configured
    }

    /// The floor it was below.
    pub fn floor(&self) -> Duration {
        self.floor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O-8's answer, pinned as a number rather than only as prose: this constant is what
    /// M7-4's migration report is measured against.
    #[test]
    fn the_floor_is_the_owners_six_hundred_seconds() {
        assert_eq!(RadioInterval::FLOOR, Duration::from_secs(600));
    }

    #[test]
    fn the_floor_is_a_property_of_every_value_of_the_type() {
        assert_eq!(
            RadioInterval::new(Duration::from_secs(600))
                .expect("at the floor")
                .get(),
            Duration::from_secs(600)
        );
        assert!(RadioInterval::new(Duration::from_secs(1800)).is_ok());

        // Today's default, which is the value this floor changes.
        let e = RadioInterval::new(Duration::from_secs(300)).expect_err("below the floor");
        assert_eq!(e.configured(), Duration::from_secs(300));
        assert_eq!(e.floor(), RadioInterval::FLOOR);
        assert!(e.to_string().contains("300 s"), "{e}");
        assert!(e.to_string().contains("600 s"), "{e}");
        assert!(e.to_string().contains("uploader.interval_seconds"), "{e}");

        assert!(RadioInterval::new(Duration::from_secs(599)).is_err());
        assert!(RadioInterval::new(Duration::ZERO).is_err());
    }

    /// The display is for a log line about a component, so it is seconds and not
    /// `Duration`'s debug form.
    #[test]
    fn an_interval_renders_as_seconds() {
        let i = RadioInterval::new(Duration::from_secs(900)).expect("above the floor");
        assert_eq!(i.to_string(), "900 s");
    }
}
