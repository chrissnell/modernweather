//! The `OnReading` path: `Ingress`, the spool, `Sink`, `SpooledSink`, and the fan-out.
//!
//! It holds *handles*, not implementations, so it does not depend on `mw-store`
//! (docs/service-architecture.md §1.3, rule 1). `Ingress::admit` does no I/O but the spool
//! append -- identity resolution reads a published snapshot through
//! [`mw_component::StationRegistry`]'s **synchronous** `resolve` and never the database
//! (§10, decision 87).
//!
//! # The shape, end to end
//!
//! ```text
//!  station components                                            the fan-out point
//!  (Serve or Every)                                                      │
//!       │ admit(batch, meta)          peers │ accept(batch, from)         │
//!       ▼                                   ▼                            ▼
//!  ┌──────────────────────────────────────────────┐          ┌──────────────────────┐
//!  │ SpoolIngress                                 │          │ Spool                │
//!  │  resolve -> authenticate -> enrich (M3-3)    │─────────▶│ append + group       │
//!  │  -> assign seq -> build Envelope             │          │ commit, segmented    │
//!  └──────────────────────────────────────────────┘          └──────────┬───────────┘
//!                                                                       │
//!            ┌──────────────────────────┬───────────────────────────────┤
//!            ▼                          ▼                               ▼
//!     durable cursors            watch<Arc<Envelope>>        broadcast(1024)
//!     writer:timescale           snapshot:latest             metrics tap
//!     uplink:<peer>              snapshot:snow, live:stream
//! ```
//!
//! **The spool is the fan-out point**, which is **D-30a** rather than an implementation
//! choice: a source may never be blocked by a sink, a bounded in-memory queue means loss
//! when full, and an unbounded one means an OOM when the database stalls — so durability
//! has to live on disk at the point where fan-out happens. `Latest` and `Lossy` subscribers
//! are notified *from the append path*, after the commit, so no subscriber can observe an
//! envelope that is not durable.
//!
//! # What is here, and what is not yet
//!
//! Here: [`spool`] (W7 §3's format and W3 §2's mechanics), [`SpoolIngress`] (`admit` and
//! `accept`, **D-60**), [`Sink`] with [`SpooledSink`], and [`SpoolStream`] — the
//! `ReadingStream` root capability, with one subscription shape per `ReadingPolicy`.
//!
//! Also here: [`rollup`] -- `window:rollup` and `rain:rollover:<station>`, which are
//! **R2**'s half of the derived values. A value that is a function of a *window* of
//! readings is maintained by a component and read from memory, never computed per request,
//! and that module is the memory plus the arithmetic.
//!
//! Also here: [`snapshot`] -- `snapshot:latest` and `snapshot:warm`, the hot path's memory.
//! `/latest` is 93% of the measured request mix and is served from an `ArcSwap` of
//! pre-serialised bytes, so a database outage no longer touches it: that module is the
//! table, the fold, and the cold-start seed.
//!
//! Not yet, each with the item that owns it: `snow:*` (M6-3) and `live:stream`, which W3
//! §2a.2's SSE surface needs and which no landed item serves -- M4-3 built the listener it
//! would mount on and deliberately did not invent a consumer for it. The snapshot payload
//! **renderer** landed with M4-2, which exclusively owns the frozen DTO. The seams
//! they attach to are marked where they land — `ingress.rs` names enrich at the line it
//! goes on — because a list in a module doc is not a work item
//! (docs/reconciliation.md G-5).

pub mod clock;
pub mod enrich;
pub mod metrics;
pub mod rollup;
pub mod snapshot;
pub mod spool;
pub mod testing;

mod ingress;
mod sink;
mod stream;
pub mod writer;

pub use clock::{Clock, SystemClock, TestClock};
pub use enrich::enrich;
pub use ingress::{
    receive_check, IngressStats, SpoolIngress, MAX_BATCH_BYTES, MAX_BATCH_ENVELOPES,
};
pub use rollup::state::{Boundaries, Rollover, RollupState, Windows};
pub use rollup::{RollupConfig, RollupPlanError};
pub use sink::{Acked, Never, OverAck, RecordingSink, Sink, SpooledSink};
pub use snapshot::state::SnapshotState;
pub use snapshot::{SnapshotConfig, SnapshotPlanError};
pub use spool::{
    Committed, Cursor, CursorPosition, CursorState, Draft, Spool, SpoolConfig, SpoolError,
    SpoolStats, CEILING_BYTES, GROUP_COMMIT_APPENDS, GROUP_COMMIT_WINDOW, LOSSY_CAPACITY, MAX_LAG,
    READ_AHEAD, SEGMENT_BYTES,
};
pub use stream::SpoolStream;
