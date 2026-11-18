//! What this crate's registrars read out of the binary's configuration type.
//!
//! One trait per concern, for `mw_station::config`'s reason: `register` is generic over the
//! configuration type and the binary chooses it (docs/service-architecture.md §1.3, rule 2),
//! so a registrar takes `&C`. The **rows** are declared by the crate that owns what they
//! are — [`IngestRow`] here for the one table no other crate reads — and the `SELECT` stays
//! in `mw-store`, which may not name this crate.
//!
//! There used to be a second trait here, `SnowGaugeConfig`, because the snow gauge's driver
//! lived in this crate: the gauge spoke gRPC and `ci/layers.toml` forbids `mw-station` from
//! naming `tonic`. Its firmware serves JSON over plain HTTP now, so the driver is
//! `mw_station::snowgauge` and the rows it reads are `mw_station::StationConfig`'s like
//! every other station's.

use std::sync::Arc;

use mw_component::Ingress;

/// One `config.ingest` row: what makes this node a collector (W7 **D-44**).
///
/// "The only thing that makes this node a collector — no flag, no mode", in the schema's own
/// words. A node with no row has no receiver, which is a satellite; a node with one has a
/// receiver, which is a collector.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IngestRow {
    /// `listen` — `grpc` or `rest`.
    pub listen: String,
    /// `peer_auth` — `mtls-ca`, `mtls-pinned` or `psk`.
    pub peer_auth: String,
    /// `ca_path`, for the two `mtls-*` kinds.
    pub ca_path: Option<String>,
    /// `allow_insecure` — docs/collector.md §5.4's explicit opt-out.
    pub allow_insecure: bool,
}

impl IngestRow {
    /// The `listen` value the gRPC receiver serves.
    pub const GRPC: &'static str = "grpc";
}

/// What `crate::grpcrx`'s registrar reads.
pub trait GrpcIngestConfig {
    /// Every `config.ingest` row.
    ///
    /// Filtering is the registrar's, so that "this node is not a collector" and "this node
    /// collects over REST" are decisions in one place.
    fn ingest(&self) -> &[IngestRow];

    /// The write end of the reading pipeline, when this process has one.
    fn ingress(&self) -> Option<&Arc<dyn Ingress>>;

    /// Whether the gRPC socket completes a TLS handshake with a client-certificate
    /// verifier (W7 §5.4 layer 1).
    ///
    /// **Added by implementation-plan.md M6-4, which is what made the answer able to be
    /// `true`.** `station:grpcrx` is mounted on that same socket, so when the collector's
    /// `peer_auth` is enforced at the handshake this method's routes are behind it too —
    /// and decision 35's opt-out stops being required. The receiver's own limitation does
    /// not go away and is restated where it belongs
    /// ([`crate::grpcrx::PEER_AUTH_NOTE`]): the handshake proves *which node* is
    /// connecting, and a `WeatherReading` carries no provenance for that to be checked
    /// against, so an authenticated peer can still write readings for any station key it
    /// knows. What changes is that an **unauthenticated** one cannot reach the method at
    /// all.
    fn ingest_tls_available(&self) -> bool;
}
