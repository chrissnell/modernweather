//! The **wire** types, and exactly two conversions.
//!
//! `prost` + `pbjson` over `generated/proto/reading.proto`, plus one
//! `TryFrom<&wire> for Reading` and one `From<&Reading> for wire`. One schema for
//! REST-JSON and gRPC-protobuf replaces the 591 hand-written mapping lines the Go version
//! carries across two conversion files (DESIGN.md D-32).
//!
//! This is the one exception to "generated output is committed": the build script runs
//! `protox` + `prost-build` into `OUT_DIR` rather than into `generated/`, because
//! `generated/proto/reading.proto` is itself drift-gated, so the prost output is a pure
//! function of a gated input and a second drift check would carry no new information
//! (docs/service-architecture.md §1.6). `protox` is a pure-Rust parser, so no `protoc`
//! binary is needed on any of the four cross targets or on a Pi.
//!
//! `protox` and `prost-build` are this crate's `[build-dependencies]`, pinned in
//! `[workspace.dependencies]`. `pbjson-build` is **not yet**: the protobuf JSON mapping has
//! no consumer until a REST surface needs it (implementation-plan.md M4-2's legacy
//! adapter, M6-4's `POST /v1/ingest`), and the item that writes those is the item that
//! knows which packages it wants `Serialize` on. What this crate carries today is the
//! binary encoding, because that is what the spool record's payload is (W7 D-11).
//!
//! # What is here
//!
//! - [`wire`] — the generated messages: `modernweather.v1.Reading` from the registry, and
//!   `collector.v1.Envelope` from `proto/collector.proto`.
//! - [`Reading`](mw_types::Reading) conversions — `reading.rs`, one list driving the
//!   encoder, the decoder and a test that fails when a registry metric is unmapped.
//! - [`Envelope`](mw_types::Envelope) conversions — `envelope.rs`, which is where a
//!   decoded provenance is re-sealed and a forged path is refused.

mod envelope;
mod reading;
pub mod wire;

pub use envelope::{EnvelopeError, MAX_STATION_KEY};
pub use reading::WireError;
