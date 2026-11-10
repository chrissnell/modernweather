//! [`WindDirCorrection`] — `config.stations.wind_dir_correction_degrees`, applied where the
//! Go drivers apply it.
//!
//! A vane is bolted to a mast and the mast is rarely aligned to north, so the correction is
//! a per-station calibration of the *sensor*, not a derived quantity. Three Go drivers
//! apply it — Davis, Campbell and Ambient (`davis/station.go:711`,
//! `campbell/station.go:239`, `ambientcustomized/station.go:397`) — and the WeatherLink Live
//! driver does not, on either implementation, which is why this is a codec input rather
//! than an enrich-stage transform: it belongs to the three drivers that have it.
//!
//! It is in this crate rather than in `mw-station` because it is arithmetic over one field
//! and the codec is where the field is produced. A correction applied one layer later would
//! be a second place a wind direction can change, and the first thing a reader would have
//! to establish is which of the two ran.
//!
//! # One departure from the Go arithmetic, in one case
//!
//! Go normalises with a loop or a single `if` depending on the driver; both are correct for
//! the range the column's own `CHECK` allows (`BETWEEN -360 AND 360`), and
//! [`WindDirCorrection::apply`] uses `rem_euclid`, which is correct for any value. The real
//! difference is the Ambient driver's `int16(windDir) + correction`, which **truncates the
//! fractional degree** an Ambient station reports — and only when a correction is
//! configured, because the zero case returns early. So a station reporting `183.5°` with a
//! correction of `+10` stores `193` under Go and `193.5` here. Faithful arithmetic in the
//! one case the two agree on, and no silent precision loss in the one they do not.

/// A per-station wind-vane offset in whole degrees.
///
/// A newtype rather than an `i16`, because "degrees of correction" and "degrees of
/// direction" are the same unit on two different things, and the one bug this calibration
/// has is adding it twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct WindDirCorrection(i16);

/// Degrees in a turn, as the type the arithmetic is done in.
const TURN: f32 = 360.0;

impl WindDirCorrection {
    /// No correction, which is the column's default and what almost every station has.
    pub const NONE: Self = Self(0);

    /// A correction in whole degrees.
    ///
    /// Any `i16` is accepted and normalised on application: the column's `CHECK` is
    /// `BETWEEN -360 AND 360` and this type is constructed from a row that may predate it,
    /// so a value outside that range is normalised rather than refused. A refusal would be
    /// a plan error for a calibration that `rem_euclid` already answers.
    #[must_use]
    pub const fn new(degrees: i16) -> Self {
        Self(degrees)
    }

    /// The configured offset, for a log field.
    #[must_use]
    pub const fn degrees(self) -> i16 {
        self.0
    }

    /// Whether this correction changes anything.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// `direction` with the offset applied, normalised to `[0, 360)`.
    ///
    /// A direction that is not a number stays what it was: `rem_euclid` of a `NaN` is a
    /// `NaN`, and the guard makes that explicit rather than depending on it. An absent
    /// direction is absent — a correction cannot invent one, which is the reason the
    /// signature takes and returns an `Option` instead of being called behind a `map`
    /// at each of three call sites.
    #[must_use]
    pub fn apply(self, direction: Option<f32>) -> Option<f32> {
        let direction = direction?;
        if !direction.is_finite() {
            return Some(direction);
        }
        if self.is_none() {
            // Verbatim, so a station with no correction stores exactly what it reported.
            // `rem_euclid` on an in-range value is the identity anyway; what this skips is
            // the one case where it is not -- a device that reports 360 for north, which
            // would become 0 for no reason an operator asked for.
            return Some(direction);
        }
        Some((direction + f32::from(self.0)).rem_euclid(TURN))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_correction_is_the_identity_including_on_the_values_normalising_would_move() {
        let none = WindDirCorrection::NONE;
        assert!(none.is_none());
        assert_eq!(none.degrees(), 0);
        for direction in [0.0, 1.5, 183.5, 359.0, 360.0, -1.0] {
            assert_eq!(none.apply(Some(direction)), Some(direction));
        }
        assert_eq!(none.apply(None), None);
    }

    #[test]
    fn a_correction_wraps_in_both_directions() {
        let plus = WindDirCorrection::new(10);
        assert_eq!(plus.apply(Some(0.0)), Some(10.0));
        assert_eq!(plus.apply(Some(355.0)), Some(5.0));
        // The Ambient case: Go truncates here and this does not.
        assert_eq!(plus.apply(Some(183.5)), Some(193.5));

        let minus = WindDirCorrection::new(-30);
        assert_eq!(minus.apply(Some(10.0)), Some(340.0));
        assert_eq!(minus.apply(Some(40.0)), Some(10.0));

        // The column's extremes, where a single `if` would still be enough and a value
        // outside the column's range would not be.
        assert_eq!(WindDirCorrection::new(360).apply(Some(359.0)), Some(359.0));
        assert_eq!(WindDirCorrection::new(-360).apply(Some(1.0)), Some(1.0));
        assert_eq!(WindDirCorrection::new(730).apply(Some(0.0)), Some(10.0));
    }

    /// A correction cannot invent a direction, and cannot turn one into a number.
    #[test]
    fn an_absent_or_unusable_direction_survives_the_correction_unchanged() {
        let c = WindDirCorrection::new(45);
        assert_eq!(c.apply(None), None);
        assert_eq!(c.apply(Some(f32::NAN)).map(f32::is_nan), Some(true));
        assert_eq!(c.apply(Some(f32::INFINITY)), Some(f32::INFINITY));
    }
}
