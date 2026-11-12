//! [`Position`] and the compression family — ten of W2a §4b.4's twelve functions.
//!
//! All of them are pure in the Go tree already and all of them sit in
//! `internal/controllers/aprs/controller.go`, beside a `*database.Client` and a dialer.
//! That is the entanglement §4b.4 is about: `aprs/controller_test.go` is 114 lines and
//! tests the worker-reconcile loop, because testing an encoder meant constructing a
//! controller with a database.
//!
//! # What changed on the way across, and why
//!
//! Three of the Go functions are wrong, and all three are **unreachable** there — nothing
//! calls `AltitudeCompress`, `CourseCompress`, `SpeedCompress`, `LatPrecompress`,
//! `LonPrecompress`, `EncodeBase91Position` or `EncodeBase91Telemetry`, because the packet
//! builder emits an *uncompressed* position report. So the defects below have never
//! transmitted anything; carrying them forward into a crate whose whole purpose is to be
//! golden-tested would have made them reachable and correct-looking.
//!
//! | Function | Go | Here |
//! |---|---|---|
//! | latitude text | `fmt.Sprintf("%2d%2.2f%v", …)` | `%02d%05.2f` |
//! | longitude text | `fmt.Sprintf("%03d%2.2f%v", …)` | `%03d%05.2f` |
//! | speed compression | `round(log(s)/log(1.08)) + 34` | `round(log(s+1)/log(1.08)) + 33` |
//!
//! The first two are the same defect twice. `%2.2f` is a *minimum width* of 2 with 2
//! decimals, so a position whose minutes are under ten renders `5.00` instead of `05.00`
//! and the field is seven characters where the protocol fixes it at eight. A receiver
//! parses the following symbol-table byte as part of the latitude and the position is
//! garbage — which the uncompressed path does hit, for any station between 45°00′ and
//! 45°10′, so it is a live defect in the one function pair that *is* reachable.
//!
//! The third is a formula transcription: the compressed speed field encodes
//! `s = 1.08^(x-33) - 1`, so the inverse is `x = log(s+1)/log(1.08) + 33`. The Go form
//! drops the `+1` and compensates with `+34`, which agrees nowhere. The protocol
//! reference's own worked example — course 88°, speed 36.2 kt, rendering as `7P` — is a
//! test below, and the Go form produces `7Q`.
//!
//! # The twelfth function
//!
//! `round(x)` in the Go controller is `math.Floor(x+0.5)` for positive input and
//! `math.Ceil(x-0.5)` otherwise, which is round-half-away-from-zero: exactly `f64::round`.
//! It is not reimplemented here. A hand-rolled copy of a standard-library rounding mode is
//! a function whose only possible behaviour is to disagree with it.

use std::fmt;

/// Base 91's radix, and the offset its digits start at.
///
/// The "funky APRS-style Base91" of the Go comment: a digit is `value + 33`, so the
/// alphabet is `!` through `{` and a field never contains a byte a TNC2 line would have to
/// escape.
const RADIX: u32 = 91;
/// The first printable byte base 91 uses.
const DIGIT0: u8 = 33;

/// The largest value two base-91 digits hold, which is the telemetry field's ceiling.
pub const MAX_TELEMETRY: u16 = (RADIX * RADIX - 1) as u16;

/// The largest value four base-91 digits hold: `91⁴ - 1`.
pub const MAX_BASE91_POSITION: u32 = RADIX * RADIX * RADIX * RADIX - 1;

/// A validated latitude and longitude, in decimal degrees.
///
/// Its own type because a position is **configuration** — `stations.latitude` and
/// `stations.longitude` — and the Go controller's readiness predicate is
/// `device.Latitude != 0 && device.Longitude != 0`, which refuses the Gulf of Guinea and
/// accepts a latitude of 91. Parsing it once, at plan time, is what makes "this station
/// cannot beacon" a configuration error with a station name in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    lat: f64,
    lon: f64,
}

impl Position {
    /// Validate a latitude and longitude.
    ///
    /// **`0, 0` is accepted**, unlike the Go predicate. It is a real position, and a
    /// station configured there is overwhelmingly likely to be one whose coordinates were
    /// never set — so the caller that cares about the difference is the registrar, which
    /// has the column's `NULL` to look at and does not need this function to guess.
    pub fn new(lat: f64, lon: f64) -> Result<Self, PositionError> {
        if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
            return Err(PositionError::Latitude { lat });
        }
        if !lon.is_finite() || !(-180.0..=180.0).contains(&lon) {
            return Err(PositionError::Longitude { lon });
        }
        Ok(Self { lat, lon })
    }

    /// Degrees north.
    pub fn latitude(self) -> f64 {
        self.lat
    }

    /// Degrees east.
    pub fn longitude(self) -> f64 {
        self.lon
    }

    /// The uncompressed latitude field: `DDMM.hhN`, eight bytes.
    pub fn latitude_text(self) -> String {
        let (degrees, minutes) = dms(self.lat);
        // `is_sign_negative` rather than `< 0.0`, so that the `-0.0` a southern field
        // parses to keeps its hemisphere: `0000.00S` would otherwise re-encode as
        // `0000.00N` and the round trip would not be total at the equator.
        let hemisphere = if self.lat.is_sign_negative() {
            'S'
        } else {
            'N'
        };
        format!("{degrees:02}{minutes:05.2}{hemisphere}")
    }

    /// The uncompressed longitude field: `DDDMM.hhW`, nine bytes.
    pub fn longitude_text(self) -> String {
        let (degrees, minutes) = dms(self.lon);
        let hemisphere = if self.lon.is_sign_negative() {
            'W'
        } else {
            'E'
        };
        format!("{degrees:03}{minutes:05.2}{hemisphere}")
    }

    /// The compressed latitude field: four base-91 digits.
    pub fn latitude_base91(self) -> [u8; 4] {
        base91_position(precompress_latitude(self.lat))
    }

    /// The compressed longitude field: four base-91 digits.
    pub fn longitude_base91(self) -> [u8; 4] {
        base91_position(precompress_longitude(self.lon))
    }
}

/// Degrees and decimal minutes, from decimal degrees.
///
/// The rounding carry is why this is one function rather than two expressions. Minutes are
/// rendered to two decimals, so a position at 45.99999° has 59.9994 minutes, which rounds
/// to `60.00` — eight bytes that are not a latitude. The carry makes it `46°00.00′`.
fn dms(degrees: f64) -> (u32, f64) {
    let magnitude = degrees.abs();
    let whole = magnitude.trunc();
    // Rounded here rather than by the formatter, so the carry is decidable.
    let minutes = ((magnitude - whole) * 60.0 * 100.0).round() / 100.0;
    if minutes >= 60.0 {
        (whole as u32 + 1, 0.0)
    } else {
        (whole as u32, minutes)
    }
}

/// Pre-compress a latitude for base-91 conversion: `380926 * (90 - lat)`.
pub fn precompress_latitude(lat: f64) -> f64 {
    380_926.0 * (90.0 - lat)
}

/// Pre-compress a longitude for base-91 conversion: `190463 * (180 + lon)`.
pub fn precompress_longitude(lon: f64) -> f64 {
    190_463.0 * (180.0 + lon)
}

/// Four base-91 digits, most significant first.
///
/// Takes the pre-compressed `f64` and truncates it, which is the conversion the Go call
/// site would have had to perform and did not express — `EncodeBase91Position(l int)` with
/// an `f64` argument at every caller. Truncation rather than rounding, because the protocol
/// reference's worked example (longitude -72.75°, pre-compressing to 20,427,156.75) renders
/// `<*e7`, which is the truncated value.
///
/// **Saturating at what four digits can hold**, which is [`MAX_BASE91_POSITION`] and not
/// `u32::MAX`: the loop below is four `% 91`s, so a value above `91⁴ - 1` wraps modulo
/// `91⁴` rather than saturating — a clamp at `u32::MAX` is 62 full wraps wide and lets the
/// south pole encode as somewhere near the north one.
///
/// The floor is `0`, and `0` is a real encoding rather than a refusal: for a latitude it is
/// 90°N. There is no representable "out of range" in four base-91 digits, so a negative
/// input saturates to the pole — which is stated here rather than claimed to be prevented.
/// Both ends are unreachable from this crate's own surface, because every caller
/// pre-compresses a validated [`Position`].
pub fn base91_position(precompressed: f64) -> [u8; 4] {
    let mut value = precompressed
        .trunc()
        .clamp(0.0, f64::from(MAX_BASE91_POSITION)) as u32;
    let mut out = [0u8; 4];
    for cell in out.iter_mut().rev() {
        *cell = (value % RADIX) as u8 + DIGIT0;
        value /= RADIX;
    }
    out
}

/// Two base-91 digits, for a telemetry channel value.
///
/// The ceiling is [`MAX_TELEMETRY`] and above it there is nothing to encode, which is why
/// this is the one member of the family that answers a `Result` — the same shape the Go
/// function has, and the only validation it performs.
pub fn base91_telemetry(value: u16) -> Result<[u8; 2], CompressError> {
    if value > MAX_TELEMETRY {
        return Err(CompressError::Telemetry { value });
    }
    let v = u32::from(value);
    Ok([(v / RADIX) as u8 + DIGIT0, (v % RADIX) as u8 + DIGIT0])
}

/// Compress an altitude in feet: `1.002^x = altitude`, as two base-91 digits.
///
/// `Err` for an altitude at or below sea level, where the logarithm is not defined — the Go
/// function takes `math.Log` of it and writes whatever `int(NaN)` produces into the packet.
/// A station below sea level is a real configuration (the Dead Sea, Death Valley) and this
/// encoding genuinely cannot express it, so it is a named refusal rather than two
/// arbitrary bytes.
pub fn compress_altitude(feet: f64) -> Result<[u8; 2], CompressError> {
    if !feet.is_finite() || feet <= 0.0 {
        return Err(CompressError::Altitude { feet });
    }
    let precompressed = (feet.ln() / 1.002_f64.ln() + 0.5).trunc();
    let value = precompressed.clamp(0.0, f64::from(MAX_TELEMETRY)) as u32;
    Ok([
        (value / RADIX) as u8 + DIGIT0,
        (value % RADIX) as u8 + DIGIT0,
    ])
}

/// Compress a course in degrees: one base-91 digit, `course = (x - 33) * 4`.
///
/// Clamped to 0–360 rather than refused: a course is a reported sensor value on this path
/// rather than configuration, and a vane that reports 361 should beacon a position with a
/// wrapped course rather than beacon nothing. `0` encodes "course unknown", which is the
/// protocol's own convention.
pub fn compress_course(degrees: u16) -> u8 {
    let clamped = f64::from(degrees.min(360));
    (clamped / 4.0 + 0.5).floor() as u8 + DIGIT0
}

/// Compress a speed in knots: one base-91 digit, `speed = 1.08^(x - 33) - 1`.
///
/// Zero and anything non-finite encode as the base digit, which is a speed of zero. See
/// this module's documentation for why the exponent is not the Go form's.
pub fn compress_speed(knots: f64) -> u8 {
    if !knots.is_finite() || knots <= 0.0 {
        return DIGIT0;
    }
    let x = ((knots + 1.0).ln() / 1.08_f64.ln()).round();
    // 90 is the largest base-91 digit; 1.08^90 - 1 is ~1 000 knots, so the clamp is
    // reachable only by a sensor fault.
    (x.clamp(0.0, 90.0) as u8) + DIGIT0
}

/// Why a position is not one.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum PositionError {
    /// Outside -90..90, or not a number.
    #[error(
        "a latitude of {lat} is not a position: APRS beacons degrees north between -90 \
         and 90. Set `config.stations.latitude` for this station."
    )]
    Latitude {
        /// What was configured.
        lat: f64,
    },

    /// Outside -180..180, or not a number.
    #[error(
        "a longitude of {lon} is not a position: APRS beacons degrees east between -180 \
         and 180. Set `config.stations.longitude` for this station."
    )]
    Longitude {
        /// What was configured.
        lon: f64,
    },
}

/// Why a value could not be compressed.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum CompressError {
    /// At or below sea level, or not a number.
    #[error(
        "an altitude of {feet} ft cannot be compressed: the encoding is `1.002^x`, which \
         has no value at or below zero. A station below sea level has to beacon an \
         uncompressed position."
    )]
    Altitude {
        /// What was asked for.
        feet: f64,
    },

    /// Above the two-digit ceiling.
    #[error(
        "a telemetry value of {value} does not fit two base-91 digits, whose ceiling is \
         8280. Scale the channel at the sender."
    )]
    Telemetry {
        /// What was asked for.
        value: u16,
    },
}

impl fmt::Display for Position {
    /// The two fields, as a packet carries them. For a log line and a test message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.latitude_text(), self.longitude_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(lat: f64, lon: f64) -> Position {
        Position::new(lat, lon).expect("a position")
    }

    /// **The live defect**, and the reason the format strings changed. Any station between
    /// 45°00′ and 45°10′ renders a seven-byte latitude in the Go builder, so the
    /// symbol-table byte that follows is parsed as part of the position.
    #[test]
    fn a_position_field_is_a_fixed_width_whatever_the_minutes_are() {
        // 45.1° is 45°06.00′: the minutes are under ten and must still be `06.00`.
        let p = at(45.1, -122.05);
        assert_eq!(p.latitude_text(), "4506.00N");
        assert_eq!(p.longitude_text(), "12203.00W");
        assert_eq!(p.latitude_text().len(), 8);
        assert_eq!(p.longitude_text().len(), 9);

        // Every hemisphere and both single-digit cases.
        assert_eq!(at(5.5, 9.25).latitude_text(), "0530.00N");
        assert_eq!(at(5.5, 9.25).longitude_text(), "00915.00E");
        assert_eq!(at(-33.75, -70.5).latitude_text(), "3345.00S");
        assert_eq!(at(-33.75, -70.5).longitude_text(), "07030.00W");
        for p in [at(0.0, 0.0), at(90.0, 180.0), at(-90.0, -180.0)] {
            assert_eq!(p.latitude_text().len(), 8, "{p}");
            assert_eq!(p.longitude_text().len(), 9, "{p}");
        }
    }

    /// **Four base-91 digits saturate at `91⁴ - 1`, not at `u32::MAX`.**
    ///
    /// The conversion is four `% 91`s, so clamping at `u32::MAX` left 62 full wraps inside
    /// the clamp: the south pole's pre-compressed latitude is well under the ceiling, but
    /// anything above `91⁴ - 1` came out as a position somewhere else entirely rather than
    /// as the extreme the clamp was supposed to pin it to.
    #[test]
    fn a_base91_position_saturates_at_what_four_digits_hold() {
        assert_eq!(MAX_BASE91_POSITION, 91 * 91 * 91 * 91 - 1);
        let ceiling = base91_position(f64::from(MAX_BASE91_POSITION));
        assert_eq!(ceiling, [DIGIT0 + 90; 4], "the all-nines of base 91");

        // One past the ceiling is the ceiling, and so is a value 62 wraps beyond it. Under
        // the old clamp the second answered `!!!!`, which is zero -- the north pole.
        assert_eq!(
            base91_position(f64::from(MAX_BASE91_POSITION) + 1.0),
            ceiling
        );
        assert_eq!(base91_position(f64::from(u32::MAX)), ceiling);
        assert_eq!(base91_position(f64::MAX), ceiling);

        // The floor is zero, and zero is an encoding rather than a refusal: 90°N.
        assert_eq!(base91_position(-1.0), [DIGIT0; 4]);
        assert_eq!(
            base91_position(precompress_latitude(90.0)),
            [DIGIT0; 4],
            "90N pre-compresses to 0, which is what a negative input saturates to"
        );

        // Both real poles stay inside the range, which is why neither end is reachable
        // from a validated `Position`.
        for degrees in [-90.0, 90.0] {
            assert!(precompress_latitude(degrees) <= f64::from(MAX_BASE91_POSITION));
        }
        for degrees in [-180.0, 180.0] {
            assert!(precompress_longitude(degrees) <= f64::from(MAX_BASE91_POSITION));
        }
        // And the protocol reference's worked example still renders as it did.
        assert_eq!(&base91_position(20_427_156.75), b"<*e7");
    }

    /// The rounding carry: 59.9994 minutes renders as `60.00` without it, which is eight
    /// bytes that are not a latitude.
    #[test]
    fn minutes_that_round_to_sixty_carry_into_the_degrees() {
        assert_eq!(at(45.99999, -121.99999).latitude_text(), "4600.00N");
        assert_eq!(at(45.99999, -121.99999).longitude_text(), "12200.00W");
        // And the ordinary case is unaffected.
        assert_eq!(at(45.9, 0.0).latitude_text(), "4554.00N");
    }

    /// The protocol reference's own worked example: 49.5°N, 72.75°W compresses to
    /// `5L!!<*e7`.
    #[test]
    fn the_compressed_position_matches_the_protocol_references_example() {
        let p = at(49.5, -72.75);
        assert_eq!(&p.latitude_base91(), b"5L!!");
        assert_eq!(&p.longitude_base91(), b"<*e7");
        // Every digit is printable, which is what the `+ 33` offset exists for.
        for b in p
            .latitude_base91()
            .iter()
            .chain(p.longitude_base91().iter())
        {
            assert!((33..=123).contains(b), "{b} is not a base-91 digit");
        }
    }

    /// The same example's course and speed — 88° and 36.2 knots render as `7P`. The Go
    /// formula produces `7Q`, which is the transcription this module's documentation
    /// describes.
    #[test]
    fn the_compressed_course_and_speed_match_the_worked_example() {
        assert_eq!(compress_course(88), b'7');
        assert_eq!(compress_speed(36.2), b'P');
        // The Go form, for the record: `round(ln(36.2)/ln(1.08)) + 34`.
        let go = ((36.2_f64.ln() / 1.08_f64.ln()).round() as u8) + 34;
        assert_eq!(go, b'Q');
        assert_ne!(go, compress_speed(36.2));
    }

    /// The spec's altitude example: 10 004 feet compresses to `S]`.
    #[test]
    fn the_compressed_altitude_matches_the_worked_example() {
        assert_eq!(&compress_altitude(10_004.0).expect("compresses"), b"S]");
        // Sea level and below have no logarithm, so they are a refusal rather than two
        // arbitrary bytes.
        for feet in [0.0, -282.0, f64::NAN, f64::NEG_INFINITY] {
            assert!(
                matches!(compress_altitude(feet), Err(CompressError::Altitude { .. })),
                "{feet} should not compress"
            );
        }
    }

    /// The course and speed digits stay inside base 91 across the whole input range,
    /// including the ones a faulty sensor produces.
    #[test]
    fn course_and_speed_stay_inside_the_alphabet() {
        for degrees in (0..=400).step_by(7) {
            let d = compress_course(degrees);
            assert!((33..=123).contains(&d), "{degrees} -> {d}");
        }
        assert_eq!(compress_course(0), DIGIT0, "zero is `course unknown`");
        assert_eq!(compress_course(360), compress_course(400), "clamped at 360");

        for knots in [0.0, 0.5, 1.0, 2.0, 36.2, 100.0, 999.0, 1.0e9, f64::NAN] {
            let d = compress_speed(knots);
            assert!((33..=123).contains(&d), "{knots} -> {d}");
        }
        assert_eq!(compress_speed(0.0), DIGIT0);
        assert_eq!(compress_speed(f64::NAN), DIGIT0);
        // Monotonic over the useful range, which a dropped `+1` would not be at the bottom.
        let mut last = compress_speed(0.0);
        for knots in 1..200 {
            let d = compress_speed(f64::from(knots));
            assert!(d >= last, "{knots} kt went backwards");
            last = d;
        }
    }

    /// Two base-91 digits hold 0..=8280 and refuse 8281, which is the Go function's one
    /// check and the only part of it that was right.
    #[test]
    fn telemetry_is_two_digits_with_a_named_ceiling() {
        assert_eq!(&base91_telemetry(0).expect("encodes"), b"!!");
        assert_eq!(&base91_telemetry(90).expect("encodes"), b"!{");
        assert_eq!(&base91_telemetry(91).expect("encodes"), b"\"!");
        assert_eq!(MAX_TELEMETRY, 8280);
        assert_eq!(&base91_telemetry(8280).expect("encodes"), b"{{");
        assert!(matches!(
            base91_telemetry(8281),
            Err(CompressError::Telemetry { value: 8281 })
        ));
    }

    /// The two pre-compression formulas, against the same worked example, so a sign error
    /// is caught where it is introduced rather than four digits later.
    #[test]
    fn the_precompression_formulas_are_the_protocols() {
        assert_eq!(precompress_latitude(49.5), 380_926.0 * 40.5);
        assert_eq!(precompress_longitude(-72.75), 190_463.0 * 107.25);
        // The poles and the antimeridian are the ends of the encodable range.
        assert_eq!(precompress_latitude(90.0), 0.0);
        assert_eq!(precompress_longitude(-180.0), 0.0);
        assert!(precompress_latitude(-90.0) < f64::from(u32::MAX));
        assert!(precompress_longitude(180.0) < f64::from(u32::MAX));
    }

    /// An impossible position is a refusal that names the value and the column.
    #[test]
    fn a_position_outside_the_globe_is_refused() {
        assert!(matches!(
            Position::new(91.0, 0.0),
            Err(PositionError::Latitude { .. })
        ));
        assert!(matches!(
            Position::new(0.0, 181.0),
            Err(PositionError::Longitude { .. })
        ));
        assert!(matches!(
            Position::new(f64::NAN, 0.0),
            Err(PositionError::Latitude { .. })
        ));
        let e = Position::new(0.0, f64::INFINITY).expect_err("refused");
        assert!(e.to_string().contains("config.stations.longitude"), "{e}");
        // `0, 0` is a real position, which the Go readiness predicate refuses.
        assert!(Position::new(0.0, 0.0).is_ok());
    }
}
