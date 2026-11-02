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
