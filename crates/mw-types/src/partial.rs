//! What a codec hands to admission, before there is a station or a sequence number.
//!
//! **Why it is a distinct type from [`Reading`] and not just one with most fields `None`.**
//! A `Reading` carries four dimensions, and two of them — `time` and `station_id` — are the
//! facts a codec is structurally unable to know: `station_id` comes from identity
//! resolution and nothing else produces a [`crate::StationKey`] (W2a §4a.6), and `time` is
//! the admitted observation instant, which is `observed_at` if the frame carried one and
//! the arrival time if it did not. A codec that could fill either would be a codec that
//! could authenticate a frame or read a clock, and `ci/layers.toml` forbids it the clock.
//!
//! So a codec fills `values`' *metrics* and says what it knows about *when*, and
//! `Ingress::admit` supplies the rest. This type is the shape of that division, which is
//! why it lives in the lowest crate: `mw-codec`'s only permitted workspace dependency is
//! this one.

use chrono::{DateTime, Utc};

use crate::Reading;

/// One decoded observation: the metrics a frame carried, and when the station says it
/// measured them.
///
/// `Vec<PartialReading>` is a `Codec::decode`'s output (docs/component-model.md §4a.2) —
/// plural because one frame can carry several observations, which is what a Davis
/// `LOOP n` response and a batched vendor-cloud poll both are.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PartialReading {
    /// When the station says it measured, when the frame says so at all.
    ///
    /// `None` is the ordinary case for a device that reports a current value with no
    /// timestamp — a Davis LOOP packet, an Ambient query string. Admission substitutes the
    /// frame's arrival time and the substitution happens in exactly one place.
    pub observed_at: Option<DateTime<Utc>>,

    /// The metrics this observation carried.
    ///
    /// Every metric the station did not report is `None` and **stays** `None` all the way
    /// to a SQL `NULL` (**D-3**, **D-26**): "the sensor did not report" is not the same
    /// fact as "the sensor reported zero", and the only place a zero is substituted is the
    /// frozen DTO's `zf` (docs/service-architecture.md §1.7).
    ///
    /// `values.time` and `values.station_id` are ignored by admission, which overwrites
    /// both. They are present because `Reading` is one generated type with no partial
    /// variant, and the drift gate is what keeps it that way.
    pub values: Reading,
}

impl PartialReading {
    /// A decoded observation with no station-reported timestamp.
    pub fn new(values: Reading) -> Self {
        Self {
            observed_at: None,
            values,
        }
    }

    /// A decoded observation the frame timestamped.
    pub fn at(observed_at: DateTime<Utc>, values: Reading) -> Self {
        Self {
            observed_at: Some(observed_at),
            values,
        }
    }
}
