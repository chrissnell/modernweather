//! Pure APRS/CWOP protocol.
//!
//! Information-field encode *and* decode, base-91 position and telemetry,
//! course/speed/altitude compression, and the passcode -- the twelve functions W2a §4b.4
//! moves out of the Go controller, where they sit entangled with database access and a
//! transport. That entanglement is why `aprs/controller_test.go` is 114 lines; here they
//! are golden-testable against captured packets, because this crate cannot link a
//! database or a transport at all (docs/component-model.md §4b.4).
//!
//! # The twelve functions, and where each one landed
//!
//! | `internal/controllers/aprs/controller.go` | Here |
//! |---|---|
//! | `CreateCompleteWeatherReport` | [`WeatherReport::encode_info`] plus `mw_output::frame`'s TNC2 header |
//! | `buildWeatherReportInfo` | [`WeatherReport::encode_info`] |
//! | `convertLatitudeToAPRSFormat` | [`Position::latitude_text`] |
//! | `convertLongitudeToAPRSFormat` | [`Position::longitude_text`] |
//! | `AltitudeCompress` | [`position::compress_altitude`] |
//! | `CourseCompress` | [`position::compress_course`] |
//! | `SpeedCompress` | [`position::compress_speed`] |
//! | `LatPrecompress` | [`position::precompress_latitude`] |
//! | `LonPrecompress` | [`position::precompress_longitude`] |
//! | `EncodeBase91Position` | [`position::base91_position`] |
//! | `EncodeBase91Telemetry` | [`position::base91_telemetry`] |
//! | `round` | `f64::round`, and [`position`] has why it is not reimplemented |
//!
//! `pkg/aprs`'s own two functions come too: `CalculatePasscode` is
//! [`Passcode::for_callsign`], and `CalculatePasscodeDebug` is deleted — a second copy of
//! the same XOR whose only difference is twelve calls that narrate the arithmetic to
//! standard output, including the credential. `ci/layers.toml`'s `no-stdout-logging` rule
//! forbids that path outright, and a second implementation of a one-line XOR is a second
//! place for it to be wrong.
//!
//! # Three defects did not come across
//!
//! Each is recorded where it lived, with the case that exercises it:
//!
//! - **The position fields are the wrong width** below ten minutes, because `%2.2f` is a
//!   minimum width of two rather than a zero-padded five ([`position`]). This one is live:
//!   it is in the function pair the packet builder actually calls.
//! - **An absent reading beacons zero**, because the builder formats a non-nullable struct
//!   ([`weather`]). `t000` is −18 °C and no receiver can tell it from a reading.
//! - **The compressed-speed exponent disagrees with its own comment** ([`position`]).
//!   Unreachable in Go, and carrying it forward into a golden-tested crate would have made
//!   it reachable and correct-looking.
//!
//! # What this crate cannot do, structurally
//!
//! `ci/layers.toml` gives it no async runtime, no database driver and no HTTP or gRPC
//! server, and additionally forbids its **source** four substrings: a clock read, the
//! standard library's networking and filesystem modules, and any path into the runtime.
//! Dev-dependencies are included in that, which is W2a assumption 7's requirement that a
//! golden test for this encoder link without a database or a transport.
//!
//! Two consequences are visible in the code rather than only in the rule. There is **no
//! clock**: every instant a packet might carry comes from a caller, and the weather values
//! come from `mw_component::WindowedView` three layers up. And the golden test's fixtures
//! are **embedded at compile time** rather than read at run time, because reading a file is
//! one of the four things this crate's source may not do — which also means the captures
//! are part of the binary that asserts against them rather than a path that could be stale.

mod callsign;
mod passcode;
pub mod position;
pub mod weather;

pub use callsign::{Callsign, CallsignError};
pub use passcode::{Passcode, PasscodeError};
pub use position::{Position, PositionError};
pub use weather::{
    parse_info, EncodeError, InformationField, ParseError, Parsed, Symbol, SymbolError,
    WeatherReport, MAX_INFORMATION_BYTES,
};
