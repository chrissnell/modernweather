//! The prost-generated messages, adopted here and nowhere else.
//!
//! Two schemas, two packages: `modernweather.v1` from `generated/proto/reading.proto`
//! (W2's emitter) and `collector.v1` from this crate's own `proto/collector.proto`. The
//! package nesting is kept, because prost writes a cross-package reference as a relative
//! path — `collector.v1.Envelope.reading` is generated as
//! `super::super::modernweather::v1::Reading` — so flattening the two into one module is a
//! compile error rather than a style choice. The two names the rest of the workspace uses
//! are re-exported below.
//!
//! `include!` rather than a path attribute: `ci/layers.toml`'s `one-seam` rule gives that
//! attribute to `mw-types` alone, because it is also the one way a module's source can live
//! where every source rule in that file is blind to it. An `include!` of `OUT_DIR` has no
//! such property — the file it names does not exist until the build script writes it, and
//! nothing hand-written can be hidden there.

// prost's output is derive-heavy generated code; the workspace's `missing_debug_implementations`
// and the documentation lints have nothing to say to it that the schema does not already say.
#![allow(missing_docs)]

/// `google.protobuf.Timestamp`, under the name the rest of the crate uses for it.
///
/// Re-exported rather than re-generated: `prost_build::Config::compile_well_known_types`
/// would emit a second copy of the well-known types into this module, and then
/// `reading.proto`'s `Timestamp` and every other crate's would be different types.
pub use prost_types::Timestamp;

/// `generated/proto/reading.proto`, emitted from `metrics/registry.toml`.
pub mod modernweather {
    /// Version 1 of the reading schema.
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/modernweather.v1.rs"));
    }
}

/// `proto/collector.proto` — the node-to-node schema.
pub mod collector {
    /// Version 1 of the collector schema.
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/collector.v1.rs"));
    }
}

pub use collector::v1::Envelope;
pub use modernweather::v1::Reading;
