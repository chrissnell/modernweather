//! Pure frame -> `Vec<PartialReading>` decoders.
//!
//! Davis LOOP, Campbell ASCII, WeatherLink Live conditions, the Ambient query string,
//! AirGradient JSON and the snow gauge's two JSON endpoints. No runtime, no socket, no
//! filesystem and **no clock**: a codec that can read the time is not golden-testable,
//! which is the whole reason these are a crate of their own rather than functions inside a
//! driver (docs/service-architecture.md §1.2). `cargo xtask gate layout` enforces all four.
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
//! **Nothing here is protobuf.** The snow gauge was, and `prost` was a dependency of this
//! crate for the two scalar fields of its `snowgauge.Reading`; its firmware now serves JSON
//! over plain HTTP ([`snowgauge`]), so the dependency, the hand-declared message and
//! `CodecError::Protobuf` went with the gRPC driver. `mw_proto::wire::Reading` is still
//! protobuf and is still generated, and it is decoded in `mw-grpc` rather than here.
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

mod airgradient;
mod ambient;
mod campbell;
mod crc16;
mod davis;
mod frame;
mod query;
mod snowgauge;
mod winddir;
mod wll;

pub use airgradient::{AirGradientJson, MEASURES_CURRENT};
pub use ambient::{
    observed_at as ambient_observed_at, params as ambient_params, rain_since, total_rain,
    AmbientQuery, ACTION_PARAM, ACTION_UPDATERAW, DATE_UTC_PARAM, ID_PARAM, PASSWORD_PARAM,
    TOTAL_RAIN_PARAM, WIND_DIR_PARAM,
};
pub use campbell::CampbellAscii;
pub use crc16::{check as crc16_check, crc16};
pub use davis::{DavisLoop, CLICKS_PER_INCH, LOOP_BYTES, LOOP_HEADER};
pub use frame::{Codec, CodecError, Frame};
pub use query::QueryParams;
pub use snowgauge::{
    snow_calibration, snow_data, snow_health, SnowCalibration, SnowData, SnowGaugeData, SnowHealth,
    SnowLiveness, SNOW_CALIBRATION, SNOW_DATA, SNOW_HEALTH, SNOW_STATUS_NO_DATA, SNOW_STATUS_OK,
};
pub use winddir::WindDirCorrection;
pub use wll::{
    real_time, Broadcast, MappingError, SensorEntry, SensorMapping, SensorRole, WllConditions,
    MAX_DSN_BYTES,
};
