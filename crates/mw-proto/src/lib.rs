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
//! `protox`, `prost-build` and `pbjson-build` are this crate's `[build-dependencies]`;
//! they are pinned in `[workspace.dependencies]` and arrive in this manifest with the
//! build script itself, which is not part of M2-1's stub.
