//! `WeatherV1` and `WeatherAppsV1` as `tonic` services mounted on the shared router.
//!
//! gRPC is a codec, not a kind of component (DESIGN.md D-31), so this crate implements
//! services and **cannot bind** -- `mw-http` owns the socket and gRPC shares it by ALPN
//! and a `content-type` test rather than by byte sniffing (§6, decision 72). It holds
//! capability handles, so it may not depend on `mw-store`.
//!
//! # What is here, and why the one station driver is
//!
//! | Module | What |
//! |---|---|
//! | [`codec`] | the one `tonic` codec, and why it is not generated |
//! | [`grpcrx`] | `station:grpcrx` — `WeatherV1.SendWeatherReadings`, the inbound one |
//!
//! **One of the station drivers lives in this crate rather than in `mw-station`**, and that
//! is `ci/layers.toml` rather than a choice: `mw-station` may not name `tonic`, because §1.4
//! gives the gRPC socket and every gRPC service to `mw-http` and this crate.
//!
//! **There used to be two.** The snow gauge was `GrpcOut { streaming: true }` — we dialled
//! it and it streamed `snowgauge.Reading` — so its body was here and its names, its row
//! validation and its codec were in `mw-station` and `mw-codec`, split across three crates
//! by that same layer rule. Its firmware now serves JSON over plain HTTP, so the whole
//! driver is `mw_station::snowgauge` and nothing of it is left here. What that removes
//! besides a module: a second station registrar in the binary, a second function that had
//! to mint the same component id, and this crate's only *outbound* gRPC channel.
//!
//! `grpc:weatherv1` and `grpc:weatherappsv1` — the *read* services the iOS client uses —
//! are still absent. They are not station drivers and nothing in M6-1 needs them.

pub mod codec;
pub mod config;
pub mod grpcrx;

pub use codec::ReadingCodec;
pub use config::{GrpcIngestConfig, IngestRow};
