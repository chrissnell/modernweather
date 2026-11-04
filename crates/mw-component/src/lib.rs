//! The component model as code: `ComponentSpec`, `Trigger`, `Capability`,
//! `Need`/`Provision`, `Tick`/`OnReading`/`Serve`, `Cx`, the lifecycle states,
//! `ComponentError` -- **and every capability handle trait**.
//!
//! The handle traits live here and their implementations live in the providing crate
//! (docs/service-architecture.md §1.3, rule 1). That is what lets `mw-pipeline` consume
//! `ReadingStore` without depending on `mw-store`, and it is why there is no dependency
//! cycle anywhere in §1.2's table. The binary is the only place a consumer and a provider
//! meet.
//!
//! Forbidden to depend on every concrete component crate: this crate must not know that a
//! station or a database exists.
//!
//! # The shape of the model, in one place
//!
//! ```text
//! plan time (pure)                     run time (the supervisor drives all of it)
//! ----------------                     ------------------------------------------
//! ComponentSpec {                      Waiting { on } -> Starting -> Running { degraded }
//!   id, trigger, requires, provides,     |                              |
//!   identity, config_digest,             |                              +-> Finished
//!   build: Builder  ------------------>  BuildCx -> Box<dyn Tick|OnReading|Serve>
//! }                                                                    |
//!                                                            Failed -> Backoff
//! ```
//!
//! Two things are worth knowing before reading any single module:
//!
//! - **Declaration is separate from construction and comes first** (W2a §1.1). Phase 1 is
//!   pure data derived from configuration; phase 2 is a builder that runs per start
//!   attempt and whose failure is a lifecycle transition rather than a startup abort.
//! - **The vocabulary here is internal and must not reach a screen** (**D-36**).
//!   "Component `aprs:backyard` is Waiting on Capability WindowedView" tells an operator
//!   nothing; W5 §0 owns the translation. What this crate owes the management API in
//!   exchange is *structure*, which is why `State::Waiting` carries the provider and not
//!   just the capability.
//!
//! # Which capability handle traits exist yet
//!
//! `ComponentControl` and `LogControl` (this crate's own surface), `ReadingStream` and
//! `ScheduleStore` (the two the supervisor itself consumes), `Ingress` -- the write end of
//! `ReadingStream` -- and the one method of `StationRegistry` that admission needs. The
//! rest of the store handles -- `ReadingStore`, `ForecastStore`, `AlertStore`, `SnowCache`,
//! `ConfigStore`, `ConfigAdmin`, `WindowedView`, `LatestSnapshot`, `HttpRoutes` -- arrive
//! with the work items that consume them, because a handle trait is a *query set* and the
//! item that writes the queries is the item that knows them. `StationRegistry` is here
//! with one method for the same reason and will grow the rest the same way. What is fixed
//! here is the mechanism every one of them uses: [`CapabilityHandle`] associates a handle
//! type with its capability, and [`BuildCx`] resolves it **once**, in `build`.

mod body;
mod capability;
mod control;
mod cx;
mod error;
mod id;
mod identity;
mod ingress;
mod routes;
mod schedule;
mod spec;
mod state;
mod stream;
mod trigger;

pub use body::{BoxFuture, BuildFn, Builder, OnReading, ReadingBatch, Serve, Tick};
pub use capability::{
    Access, Arity, Capability, Freshness, Need, ProviderClass, Provision, ProvisionDetail, Strength,
};
pub use control::{ComponentControl, ControlError, Generation, LogControl, LogControlError};
pub use cx::{
    Additive, BuildCx, CapabilityAccess, CapabilityHandle, CapabilityPublisher, CapabilitySlot, Cx,
    Published, Publisher,
};
pub use error::{BuildError, ComponentError, Outcome, PlanError};
pub use id::{stable_hash, ComponentId, ComponentIdError, Fnv1a};
pub use identity::{
    Cardinality, Continuity, IdentityProblem, IdentityShape, IdentitySource, Initiation,
};
pub use ingress::{
    AcceptError, Accepted, AdmitError, Admitted, FrameMeta, Ingress, Reject, StationRegistry,
};
pub use routes::{Match, Methods, RouteEntry, RouteManifest, Scope};
pub use schedule::{OccurrenceKey, OccurrenceRecord, ScheduleStore, ScheduleStoreError};
pub use spec::{ComponentSpec, ConfigDigest, SpecFingerprint, DEFAULT_GRACE};
pub use state::{ComponentStatus, Counters, ScheduleState, State, WaitingOn};
pub use stream::{ReadingStream, ReadingSubscription, StreamError};
pub use trigger::{
    BodyKind, CronProblem, CronSpec, FirstRun, LocalTime, Misfire, Phase, ReadingFilter,
    ReadingPolicy, StationSet, Trigger, TzSource, Zone, ZoneError,
};
