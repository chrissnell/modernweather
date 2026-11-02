//! `WeatherV1` and `WeatherAppsV1` as `tonic` services mounted on the shared router.
//!
//! gRPC is a codec, not a kind of component (DESIGN.md D-31), so this crate implements
//! services and **cannot bind** -- `mw-http` owns the socket and gRPC shares it by ALPN
//! and a `content-type` test rather than by byte sniffing (§6, decision 72). It holds
//! capability handles, so it may not depend on `mw-store`.
