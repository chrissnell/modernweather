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
//!
//! # What is here
//!
//! [`WllConditions`], the WeatherLink Live codec, and the [`Frame`]/[`Codec`] pair every
//! other one will implement. The remaining five arrive with the items that need them —
//! Davis and Campbell with M6-1, Ambient and AirGradient with M6-1, the snow gauge with
//! M6-3 — because a decoder with no driver is a decoder with no oracle.
//!
//! # The division of labour this crate's purity rests on
//!
//! ```text
//!  bytes          Frame            PartialReading           Envelope
//!  ─────▶ Session ─────▶ Codec ──────────────▶ Ingress::admit ─────▶ spool
//!         (impure)      (HERE)                 (identity, seq,
//!                                               enrich, durability)
//! ```
//!
//! A [`PartialReading`](mw_types::PartialReading) is what a codec can honestly produce: the
//! metrics the frame carried, and the instant the frame said it measured them. It cannot
//! produce a station — identity resolution is the only producer of a
//! [`StationKey`](mw_types::StationKey) (W2a §4a.6) — and it cannot produce a sequence
//! number, which is a position in the spool. Both are `admit`'s, and the type is the shape
//! of that division.

mod frame;
mod wll;

pub use frame::{Codec, CodecError, Frame};
pub use wll::{
    real_time, Broadcast, MappingError, SensorEntry, SensorMapping, SensorRole, WllConditions,
};
