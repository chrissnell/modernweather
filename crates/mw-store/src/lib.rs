//! Storage: `sqlx` against PostgreSQL and SQLite.
//!
//! Owns the pools and implements `ReadingStore`, `ForecastStore`, `AlertStore`,
//! `SnowCache`, `ConfigStore`, `ConfigAdmin` and `StationRegistry` -- the capability
//! handle traits `mw-component` declares. Column names come from
//! `mw_types::AggregateColumn`, not from string literals.
//!
//! **The storage layer must not be able to name an endpoint**, so this crate may not
//! depend on `axum`, `tonic`, `mw-api-legacy` or `mw-web` (docs/service-architecture.md
//! §1.2).
//!
//! The writer stores `NULL` for a metric a station did not report. The frozen contract's
//! zeros are produced one layer later, at the DTO in `mw-api-legacy`; the two facts live
//! in two crates and cannot be confused (§1.7).

/// The configuration store and its migrations (D-8, A-W6-2).
pub mod config_db;
/// `ForecastStore` and `SnowCache`: the frozen REST surface's own two reads.
pub mod frozen;
/// Minting this node's `NodeId` and an enrolment token, with no UUID or RNG crate (W7 D-5).
pub mod ident;
/// `PeerRegistry` over `peers` and the registration path W7 §5.1 gives `ingest:*`.
pub mod peers;
/// `db:postgres`: the weather pool, its version assertion and its publish/revoke loop.
pub mod pg;
/// The read half of `ReadingStore`: the roll-up's three reads, the snapshot seed, and
/// `/span` and `/almanac`.
pub mod reader;
/// `StationRegistry` over a published snapshot of `config.stations` -- W2a §4a.6's single
/// identity-resolution gate.
pub mod registry;
/// `ScheduleStore` over `component_schedule` alone -- the supervisor's own durable state
/// (**D-41**).
pub mod schedule;
/// `TimescaleStore`: both halves of the weather pool, as the one published handle.
pub mod store;
/// The startup database-version assertion (**D-62**).
pub mod version;
/// `writer:timescale`'s batch `INSERT` and **D-56**'s read-side projections.
pub mod writer;

pub use crate::frozen::FrozenReads;
pub use crate::peers::{PeerSnapshot, PeerStore};
pub use crate::reader::TimescaleReader;
pub use crate::registry::{
    hash_ingest_secret, ConfigRegistry, RegistrySnapshot, SecretHashError, SecretProblem,
    StationEntry, ZoneProblem,
};
pub use crate::schedule::SqliteScheduleStore;
pub use crate::store::TimescaleStore;
pub use crate::writer::TimescaleWriter;
