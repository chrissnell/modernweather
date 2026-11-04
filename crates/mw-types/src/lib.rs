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
//! The hand-written domain types §1.2 adds here arrive with the work items that consume
//! them. `StationId`, `StationKey`, `WebsiteId`, `OriginId`, `Seq`, `Envelope`,
//! `Provenance` and `PathSet` are here, because implementation-plan.md M2-3 needs a name
//! for the thing a `Trigger::OnReading` component is handed and for the station a
//! `Calendar` component takes its time zone from; `Envelope::forward`, `Provenance::sealed`
//! and `PartialReading` arrive with M3-2, which owns admission. (`FrameMeta` is M3-2's too
//! and lives in `mw-component`, because the `Ingress` trait that takes it does.) `Window`,
//! `Agg` and the
//! `ext.rs` additions to `Reading` are still absent: they belong to the window rollup (M6)
//! and to the enrich stage (M3-3).

mod envelope;
mod error;
mod generated;
mod id;
mod partial;

// Every public item of the three generated modules, re-exported and nothing added. The
// list is exhaustive on purpose: the `generated` module is private, so an item that is not
// re-exported is unreachable from the rest of the workspace -- and `dead_code` says so,
// which is what keeps this list honest as the registry grows.
pub use generated::aggregates::{AggregateColumn, Level, Strategy, LEVELS};
pub use generated::metrics::{MetricKey, MetricMeta, Tier, UnitMeta, METRICS, UNITS};
pub use generated::reading::Reading;

pub use envelope::{Envelope, ForwardRefused, PathProblem, PathSet, Provenance};
pub use error::{Class, Classify, DisplayChain};
pub use id::{OriginId, Seq, StationId, StationKey, WebsiteId};
pub use partial::PartialReading;
