//! The `OnReading` path: `Ingress`, the enrich stage, `Sink`, `SpooledSink`, and the
//! `snapshot:latest`, `window:rollup`, `rain:totals`, `snow:*` and `live:stream`
//! components.
//!
//! It holds *handles*, not implementations, so it does not depend on `mw-store`
//! (docs/service-architecture.md §1.3, rule 1). `Ingress::admit` does no I/O but the spool
//! append -- identity resolution reads an `ArcSwap` of the registry and never the database
//! (§10, decision 87).
