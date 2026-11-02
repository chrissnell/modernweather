//! The entire seam between generated and hand-written code -- three declarations.
//!
//! `#[path]` rather than `include!`, deliberately (DESIGN.md D-5): `generated/rust/reading.rs`
//! opens with the inner attribute `#![allow(clippy::struct_excessive_bools)]`, and a macro
//! expansion cannot contribute an inner attribute to the enclosing module, so
//! `mod reading { include!(...) }` fails to compile. `#[path]` loads the file *as* the
//! module body and inner attributes work.
//!
//! The relative depth is from this file's directory, so `../../../../` is the repository
//! root. `reading.rs` and `aggregates.rs` both say `use super::metrics::MetricKey;`, which
//! resolves because the three are siblings under this module.
//!
//! This module is **private to `mw-types`**. The crate root re-exports the types other
//! crates need, so a future "just add a field to the generated struct" has nowhere to land
//! (docs/service-architecture.md §1.6, rule 1). Anything that would be a hand-written
//! addition to a generated type is an `impl` block in `crates/mw-types/src/ext.rs`.

// `#[rustfmt::skip]` on each declaration, because `#[path]` makes these files part of this
// crate, so `cargo fmt --all` rewrites them in the working tree -- and then
// `cargo xtask codegen --check` reports drift against the generator that wrote them. Two
// PR-gate jobs would disagree about what the file should contain, and `cargo fmt --all`
// would be the thing that broke the build. A generated file's formatting is its
// generator's output and the drift gate is what governs it, so rustfmt does not get a
// vote here.
#[rustfmt::skip]
#[path = "../../../../generated/rust/aggregates.rs"]
pub mod aggregates;
#[rustfmt::skip]
#[path = "../../../../generated/rust/metrics.rs"]
pub mod metrics;
#[rustfmt::skip]
#[path = "../../../../generated/rust/reading.rs"]
pub mod reading;
