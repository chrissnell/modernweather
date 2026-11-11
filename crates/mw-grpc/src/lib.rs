//! `WeatherV1` and `WeatherAppsV1` as `tonic` services mounted on the shared router.
//!
//! gRPC is a codec, not a kind of component (DESIGN.md D-31), so this crate implements
//! services and **cannot bind** -- `mw-http` owns the socket and gRPC shares it by ALPN
//! and a `content-type` test rather than by byte sniffing (§6, decision 72). It holds
//! capability handles, so it may not depend on `mw-store`.
//!
//! # What is here, and why the two station drivers are
//!
//! | Module | What |
//! |---|---|
//! | [`codec`] | the two `tonic` codecs, and why neither is generated |
//! | [`snowgauge`] | `station:snowgauge:<key>` — the outbound stream W2a §4a.5 maps the gauge to |
//! | [`grpcrx`] | `station:grpcrx` — `WeatherV1.SendWeatherReadings`, the inbound one |
//!
//! **Two of the seven station drivers live in this crate rather than in `mw-station`**, and
//! that is `ci/layers.toml` rather than a choice: `mw-station` may not name `tonic`, because
//! §1.4 gives the gRPC socket and every gRPC service to `mw-http` and this crate. What is
//! split is only the part that needs `tonic` — the names, the row validation, the component
//! ids and the codecs stay in `mw-station` and `mw-codec`, and
//! `mw_station::snowgauge`'s module documentation is the other half of that seam.
//!
//! `grpc:weatherv1` and `grpc:weatherappsv1` — the *read* services the iOS client uses —
//! are still absent. They are not station drivers and nothing in M6-1 needs them.

pub mod codec;
pub mod config;
pub mod grpcrx;
pub mod snowgauge;

pub use codec::{BytesCodec, ReadingCodec};
pub use config::{GrpcIngestConfig, IngestRow, SnowGaugeConfig};
