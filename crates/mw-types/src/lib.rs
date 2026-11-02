//! Domain types, and the one seam through which generated code enters the workspace.
//!
//! `generated/rust/{metrics,reading,aggregates}.rs` are adopted here and nowhere else
//! (docs/service-architecture.md §1.6, DESIGN.md D-5). The `generated` module is private:
//! what the rest of the workspace sees is the re-exports below, so "just add a field to
//! the generated struct" has nowhere to land.
//!
//! Nothing here depends on a runtime, a database, a transport or `serde`. That is what
//! keeps a legacy JSON key out of the domain type (§1.7): a `#[serde(rename = "otemp")]`
//! would have to add `serde` to this manifest first, and `cargo xtask gate layout`
//! rejects that.
//!
//! The hand-written domain types §1.2 adds here -- `StationKey`, `StationId`,
//! `WebsiteId`, `OriginId`, `Seq`, `Window`, `Agg`, `Envelope`, `Provenance` -- and the
//! `ext.rs` additions to `Reading` are not in this stub; they arrive with the work items
//! that consume them.

mod generated;

// Every public item of the three generated modules, re-exported and nothing added. The
// list is exhaustive on purpose: the `generated` module is private, so an item that is not
// re-exported is unreachable from the rest of the workspace -- and `dead_code` says so,
// which is what keeps this list honest as the registry grows.
pub use generated::aggregates::{AggregateColumn, Level, Strategy, LEVELS};
pub use generated::metrics::{MetricKey, MetricMeta, Tier, UnitMeta, METRICS, UNITS};
pub use generated::reading::Reading;
