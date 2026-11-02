//! **The one listener.**
//!
//! `axum` + `tonic` on one socket, `RouteTable` mounting, host routing, the SNI resolver,
//! ALPN, compression and CORS. The Go version has seven listener-owning sites and a single
//! type that owns an HTTP server, an HTTPS server, a gRPC server, SNI, middleware,
//! database queries and config reload; the direct structural answer is that **only this
//! crate may bind a socket**, enforced by `cargo xtask gate layout`
//! (docs/service-architecture.md §1.4, §1.5).
//!
//! It mounts route tables; it does not know what is in them. So it may not depend on
//! `mw-store`, `mw-pipeline`, `mw-api-legacy` or `mw-web`.
