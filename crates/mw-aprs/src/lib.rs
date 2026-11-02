//! Pure APRS/CWOP protocol.
//!
//! Information-field encode *and* decode, base-91 position and telemetry,
//! course/speed/altitude compression, and the passcode -- the twelve functions W2a §4b.4
//! moves out of the Go controller, where they sit entangled with database access and a
//! transport. That entanglement is why `aprs/controller_test.go` is 114 lines; here they
//! are golden-testable against captured packets, because this crate cannot link a
//! database or a transport at all (docs/component-model.md §4b.4).
