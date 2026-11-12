//! The collector: W7's uplink and gRPC receiver.
//!
//! In the Go version, sending readings to a peer is implemented as a *storage engine* and
//! receiving them is implemented as a *weather station*, and the role that actually exists
//! has no name anywhere in the codebase. It has one here (DESIGN.md D-31).
//!
//! Forbidden to depend on `mw-api-legacy` or `mw-web`.
//!
//! # The two components, and why neither is a mode
//!
//! | Component | Exists when | What it does |
//! |---|---|---|
//! | [`service`] — `ingest:grpc` | a `config.ingest` row with `listen = 'grpc'` | serves `collector.v1.Collector` and `POST /v1/ingest` |
//! | [`uplink`] — `uplink:<peer>` | an `uplink` row | reads this node's spool and forwards to one peer |
//!
//! **There is no `role`, no `mode` and no `is_collector` anywhere** (W7 **D-1**, **D-44**).
//! A collector is a node whose components include an ingest endpoint; a satellite is one
//! whose components include an uplink; a relay has both, which is why "both" needs no
//! syntax. Same binary, same code path, different configuration rows.
//!
//! # What the ack means, which is the thing this item exists for
//!
//! `Ingest(stream IngestBatch) returns (stream IngestAck)` replaces
//! `SendWeatherReadings(stream WeatherReading) returns (Empty)` — a signature under which
//! no sender could ever learn what its collector persisted, with or without a local
//! buffer. An ack means **durably appended to the collector's spool**: not "accepted", and
//! not "written to PostgreSQL" (W7 **D-21**). [`service`]'s module documentation has the
//! whole of that boundary and `tests/ack.rs` is where it is demonstrated rather than
//! claimed.
//!
//! # The three admission layers (W7 **D-34**), one module each
//!
//! | Layer | Module | Refuses |
//! |---|---|---|
//! | 1 the TLS handshake | [`handshake`] (+ [`x509`]) | an unknown key, before a frame is decoded |
//! | 2 a per-RPC check | [`service`] | an unknown, revoked or wrongly-credentialled peer — no stream is created |
//! | 3 per envelope | [`admit`] | an origin this peer may not speak for, a station it may not write into, a clock out of the window |
//!
//! The Go receiver has none of these: it configures server TLS only
//! (`grpcreceiver/station.go:118-128`), so anyone who can reach the port is a peer, and it
//! pushes the reading into the distributor at `station.go:175` before consulting the
//! registry at `:182` — so an unknown sender's reading is stored and then debug-logged.
//!
//! # The Go compat shim is not here
//!
//! `WeatherV1.SendWeatherReadings` is `mw_grpc::grpcrx`'s and stays there, under the id
//! `station:grpcrx`. W7 **D-2** renames the collector's ingest component to `ingest:grpc`
//! and that is the component in [`service`]; the shim keeps its own id because it is a
//! *different service on a different schema* with a different contract, and collapsing the
//! two would make "this peer is at-most-once" invisible. §9.2's three limitations are
//! `mw_grpc::grpcrx`'s own documentation, and what this item adds to them is the metric
//! their removal is gated on — `mw_peer_software_info`, emitted by [`metrics`] from
//! `peers.software`, which is exactly what W7 OQ-W7-6 asks for.

pub mod admit;
pub mod handshake;
pub mod metrics;
pub mod rest;
pub mod service;
pub mod testing;
pub mod uplink;
pub mod wire;
pub mod x509;

pub use admit::{Gate, Presented};
pub use handshake::{PeerAuthKind, PinnedSpki, TlsError};
pub use service::{
    CollectorConfig, Handlers, IngestPlanError, IngestRow, ServiceState, ID as INGEST_ID,
};
pub use uplink::{Identity, Uplink, UplinkConfig, UplinkError, UplinkPlanError, UplinkRow};
