//! Pure frame -> `Vec<PartialReading>` decoders.
//!
//! Davis LOOP, Campbell ASCII, WeatherLink Live conditions, the Ambient query string,
//! AirGradient JSON and the snow gauge. No runtime, no socket, no filesystem and **no
//! clock**: a codec that can read the time is not golden-testable, which is the whole
//! reason these are a crate of their own rather than functions inside a driver
//! (docs/service-architecture.md §1.2). `cargo xtask gate layout` enforces all four.
//!
//! The Davis LOOP packet is a fixed little-endian structure, so it is decoded as a struct
//! through `zerocopy` rather than through reflection-based binary reads (DESIGN.md D-32).
