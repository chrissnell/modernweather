//! `snapshot:latest` and `snapshot:warm` — the hot path's memory, and its cold start.
//!
//! ```text
//!                                     ┌──────────────────┐
//!  spool watch ── OnReading ─────────▶│ snapshot:latest  │──┐
//!  WindowedView (S) ─────────────────▶└──────────────────┘  │  Arc<dyn LatestSnapshot>
//!                                                           ├──▶ api:legacy  (M4-2)
//!  ReadingStore::latest ── OnStartup ▶┌──────────────────┐  │    web:site:*  (M4-3)
//!  WindowedView (S) ─────────────────▶│ snapshot:warm    │──┘
//!                                     └──────────────────┘
//!                            both write one Arc<SnapshotState>
//! ```
//!
//! # The two components
//!
//! | Component | Trigger | Requires | Provides |
//! |---|---|---|---|
//! | `snapshot:latest` | `OnReading { All, Latest, 100 ms }` | `WindowedView` S | `LatestSnapshot` |
//! | `snapshot:warm` | `OnStartup { 30 s }` | `ReadingStore` H, `WindowedView` S | — |
//!
//! `WindowedView` is **declared** here and **resolved through the registrar**, not off the
//! capability bus. [`SnapshotConfig::snapshot_windows`] has the whole of why; it is the
//! shape `mw_station` already uses for `ReadingStream`, and the reason is that a soft
//! handle is resolved once in `build` while this component is built before `window:rollup`
//! can have published anything.
//!
//! `StationRegistry` travels the same way and for a different reason — nothing in this
//! workspace provides `Capability::StationRegistry`, because `config:sqlite` has no work
//! item (`mw_store::pg` records the same note). `docs/component-model.md` §4's table gives
//! `snapshot:latest` `StationRegistry` **H**; declaring it would be a hard need with no
//! provider, which `plan()` refuses under **D-44**. Recorded as **A-M4-1-2**.
//!
//! # `snapshot:latest` has no hard requirement, and **D-48** is why that matters
//!
//! `api:legacy` declares only `LatestSnapshot` as a hard requirement (W3 §6.2), so whether
//! a `503` from an unmounted route can appear on a **frozen** path reduces to whether this
//! component can fail to publish. It cannot: it has no hard need, so the supervisor builds
//! it immediately, and it publishes its handle **in `build`** rather than on the first
//! successful fold. With the database down the table is empty and `/latest` answers its
//! documented `404 no recent weather data available for this station\n` — which is a
//! contract response, where a `503` would not be.
//!
//! That is the opposite of the choice `window:rollup` makes, and the difference is the
//! consumer. A `WindowedView` published before its first tick answers `UnknownStation` for
//! every station, so an uploader is better off in `Waiting` until there is something to
//! resolve. A `LatestSnapshot` published before its first fold answers `None`, which is a
//! state the frozen contract already has bytes for — and a consumer in `Waiting` is a
//! frozen route that is not mounted, which is the one thing **D-48** forbids.
//!
//! **This is the whole of M4-1's half of D-48's invariant.** The rest of it — "every frozen
//! route returns its contract response and never `503`" — needs frozen routes, and
//! `mw-api-legacy` is M4-2 while the route table and `http:listener` are M4-3. What is
//! testable here is the precondition the invariant rests on, and
//! `crates/modernweather/tests/snapshot_degraded.rs` tests it against the real supervisor
//! with no `db:postgres` at all: a consumer whose only hard need is `LatestSnapshot`
//! reaches `Running`, so its route table mounts.
//!
//! # `snapshot:warm` requires `ReadingStore` **hard**, which W3 D-37 writes as soft
//!
//! D-37: *"`snapshot:warm` is an `OnStartup` component that seeds the snapshot table from
//! the store so a restart does not 404 a station that was inside the 10-minute window; with
//! `ReadingStore` unavailable it finishes empty rather than refusing to start."* W3 §2a.6
//! spells the requirement `ReadingStore` **soft**.
//!
//! **Soft does not work, and the reason is the supervisor rather than the decision.** Only
//! hard edges form the ordering graph (W2a Decision 17), so a soft need delays nothing: an
//! `OnStartup` component with soft `ReadingStore` fires the instant it is spawned, which on
//! every node — healthy or not — is before `db:postgres` has connected and published. It
//! would therefore seed **nothing, always**, and the one thing it exists to do would never
//! happen. A soft handle also cannot be waited for after `build`: `BuildCx::additive` is
//! restricted to additive capabilities precisely so that a component cannot watch an
//! exclusive one appear (W2a Decision 21).
//!
//! So the need is **hard**, and what that changes is one thing: with the database down this
//! component sits in `Waiting { ReadingStore }` instead of reaching `Finished`. The
//! observable D-37 cares about is identical — an empty table, and `/latest` 404ing until a
//! reading arrives — because *nothing requires this component*, so `Waiting` cascades
//! nowhere and the frozen route table still mounts. And when the server comes back the seed
//! actually runs, which is strictly better than having finished empty an hour earlier.
//!
//! Recorded as **A-M4-1-1** in `docs/reconciliation.md`'s *Proposed amendments* section.
//! D-37 stands as written until it is applied.
//!
//! # What this item does not build
//!
//! **The renderer.** `/latest`'s body is the frozen 133-key DTO and `mw-api-legacy`
//! exclusively owns it (W3 D-9, D-10); this crate is a lower layer and may not name that
//! one. So [`mw_component::SnapshotRender`] is a capability-shaped trait whose
//! implementation is M4-2's, and until it lands [`SnapshotConfig::snapshot_render`] answers
//! `None` and [`register`] plans neither component — logged once, naming the item, the way
//! `mw_http`'s single route records its own scope boundary. Everything else here is
//! complete and is driven in tests by [`crate::testing::CountingRender`].
//!
//! **The snow tiers.** `/snow` is a second hot path with its own capability and its own
//! refresh rates (W3 §2a.7), and `SnowCache` has no provider: `snapshot:snow`,
//! `snow:events`, `snow:smoothing`, `snow:storm` and `snow:season` are M6-3's. So a
//! snapshot's `Staleness` can name `WindowedView` and not `SnowCache` today, which is the
//! honest set rather than a capability nothing can revoke.
//!
//! **The handler.** The four things W3 **D-34** lists — resolve the website, resolve the
//! station, apply the 10-minute rule, write `Bytes` — are M4-2's, and the two additive
//! headers **D-36** sends the staleness out through are too. What is here is everything
//! those four things read: [`mw_component::Snapshot::json`],
//! [`mw_component::Snapshot::observed_at`], [`mw_component::Snapshot::etag`] and
//! [`mw_component::Snapshot::staleness`].

pub mod state;

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mw_component::{
    BoxFuture, Builder, Capability, ComponentError, ComponentId, ComponentIdError, ComponentSpec,
    ConfigDigest, Cx, Declaration, LatestQuery, LatestSnapshot, Need, OnReading, PayloadRev,
    Provision, ReadingBatch, ReadingFilter, ReadingPolicy, ReadingStore, Resolved, Snapshot,
    SnapshotKey, SnapshotRender, SnapshotSource, Staleness, StationRegistry, Store, Tick, Trigger,
    WindowedView,
};
use mw_types::{Agg, MetricKey, Reading, StationKey, Window};
use smallvec::SmallVec;
use tracing::{info, warn};

use self::state::SnapshotState;
use crate::clock::{Clock, SystemClock};
use crate::metrics as m;

/// `snapshot:latest`'s component id.
pub const LATEST_ID: &str = "snapshot:latest";

/// `snapshot:warm`'s component id.
pub const WARM_ID: &str = "snapshot:warm";

/// `snapshot:latest`'s per-batch deadline (`docs/component-model.md` §4.1).
///
/// A hundred milliseconds for work that is a fold and one `serde_json` pass over a flat
/// 133-field struct — single-digit microseconds (W3 §2a.3). A miss here means the process
/// is hung, which is the same reasoning `writer:timescale`'s five seconds has.
pub const LATEST_DEADLINE: Duration = Duration::from_millis(100);

/// `snapshot:warm`'s one-run deadline.
///
/// One `latest` query per key — at most the cross product, 30 in W1's capture rig — each an
/// index seek on the newest chunk. Thirty seconds is two orders of magnitude of slack, and
/// it is the figure rather than something tighter because a cold pool's first statement
/// also pays for the connection.
pub const WARM_DEADLINE: Duration = Duration::from_secs(30);

/// W1 Decision 15's recency rule, which is the window `snapshot:warm` seeds from.
///
/// `fetchLatestReading` filters `time >= NOW() - INTERVAL '10 minutes'`, and `/latest` is
/// the only endpoint where staleness becomes a 404. Seeding anything older would turn a
/// documented 404 into a 200 and change what the iOS client does, so the query carries the
/// bound rather than the handler discarding the row afterwards.
pub const RECENCY: Duration = Duration::from_secs(600);

/// How stale the window provider's own `as_of` may be before a snapshot is
/// [`Staleness::Degraded`].
///
/// `window:rollup` ticks every 60 s at a 5-second phase offset, so one missed tick is
/// ~65 seconds of silence and two is ~125. **150 seconds** is past two and inside three,
/// which is the band where "the database is down" stops being indistinguishable from "the
/// tick is a little late". Below the period it would flap on every normal gap; much above
/// it the indicator arrives after an operator has already noticed.
///
/// Measured against `Resolved::as_of`, which is why that field exists — a caller cannot
/// otherwise tell an empty window from a provider that has stopped refreshing, and an
/// all-`None` answer reads as "the station does not report this".
pub const WINDOW_STALE_AFTER: Duration = Duration::from_secs(150);

/// [`WINDOW_STALE_AFTER`] as a `chrono` delta, computed once.
///
/// The comparison in [`Folder::resolve`] used to be a fallible `Duration` conversion with a
/// fallback arm computing the same number a second way — unreachable, and re-run on every
/// fold. One `const` means the figure appears once and the comparison is a subtraction.
const STALE_AFTER: chrono::TimeDelta =
    match chrono::TimeDelta::try_seconds(WINDOW_STALE_AFTER.as_secs() as i64) {
        Some(d) => d,
        // `expect` is not const; 150 seconds is representable and a change that made it
        // not would be a build failure here rather than a silent fallback.
        None => panic!("WINDOW_STALE_AFTER must be a representable TimeDelta"),
    };

/// The window-derived values the `/latest` payload carries (W3 §2a.6's table).
///
/// **R2's set, less the snow totals.** Wind gust and the rain rate come out of
/// `window:rollup`'s ring; the calendar and multi-day rainfall totals and the storm total
/// come out of the windows it maintains beside it. The snow totals are `SnowCache`'s, which
/// has no provider until M6-3.
///
/// The rain rate is `Window::PerHour(TEN_MINUTES)` with [`Agg::Sum`] rather than the
/// station's `rainrate` column, because that is what the frozen contract promises:
/// `handlers.go:344` overwrites the reported column with
/// `sum(rainincremental) over 10 minutes x 6` before serialising, so reading the column
/// would be a bytes-visible change.
pub const WINDOWS: [Declaration; 9] = [
    // Wind gust: the greatest instantaneous speed in the last ten minutes.
    (
        MetricKey::WindSpeed,
        Window::Rolling(Window::TEN_MINUTES),
        Agg::Max,
    ),
    // Rain rate: a rate, not a total. See above.
    (
        MetricKey::RainIncremental,
        Window::PerHour(Window::TEN_MINUTES),
        Agg::Sum,
    ),
    // The calendar totals, in the **station's** zone (**D-24**).
    (MetricKey::RainIncremental, Window::Day, Agg::Sum),
    (MetricKey::RainIncremental, Window::Month, Agg::Sum),
    (MetricKey::RainIncremental, Window::Year, Agg::Sum),
    // The multi-day rolling totals.
    (
        MetricKey::RainIncremental,
        Window::Rolling(Duration::from_secs(24 * 3600)),
        Agg::Sum,
    ),
    (
        MetricKey::RainIncremental,
        Window::Rolling(Duration::from_secs(48 * 3600)),
        Agg::Sum,
    ),
    (
        MetricKey::RainIncremental,
        Window::Rolling(Duration::from_secs(72 * 3600)),
        Agg::Sum,
    ),
    // The computed storm total, which is not the station-reported `storm_rain` column.
    (MetricKey::RainIncremental, Window::Storm, Agg::Sum),
];

/// What this crate needs from the configuration to plan the snapshot components.
///
/// A trait rather than a type for [`crate::RollupConfig`]'s reason: `plan()` is generic over
/// the configuration and the binary chooses it (§1.3 rule 2), so a registrar takes `&C` and
/// each component crate names only the rows it plans from.
pub trait SnapshotConfig {
    /// Whether this node has a weather database.
    ///
    /// `snapshot:warm` requires `ReadingStore` hard, and without a `weather_database` row
    /// there is no `db:postgres` to provide it — so planning the component would be a hard
    /// need with no provider and `plan()` would refuse the whole configuration (**D-44**).
    fn has_weather_database(&self) -> bool;

    /// Whether the reading stream exists, which is whether this node has a spool.
    ///
    /// `Trigger::OnReading` is validated against the root `ReadingStream`, and a node with
    /// no `node` row has no identity and therefore no spool (W7 §8.2). Without one
    /// `snapshot:latest` cannot be planned.
    fn has_reading_stream(&self) -> bool;

    /// The shared state, built by `main` as a root resource. `None` is a bug in the boot
    /// sequence rather than a configuration error; see [`SnapshotPlanError::NoState`].
    fn snapshot_state(&self) -> Option<&Arc<SnapshotState>>;

    /// The published station registry, for the static metadata a payload reads
    /// (**D-31**, W4 assumption 24).
    ///
    /// Through the registrar rather than off the capability bus, for the reason
    /// `mw_store::pg` records: `Capability::StationRegistry` has no provider because
    /// `config:sqlite` has no work item, and `main` owns the snapshot as a root resource. A
    /// `Need::hard(Capability::StationRegistry)` here would be a hard need nobody provides.
    fn snapshot_registry(&self) -> Option<&Arc<dyn StationRegistry>>;

    /// The maintained window values (**R2**), for the same reason the registry travels
    /// this way.
    ///
    /// **This is the declaration's resolution, and it may not come off the bus.** Both
    /// components declare `Need::soft(WindowedView)` and resolve **nothing** from the
    /// capability bus, which is the shape `mw_station`'s stations already have for
    /// `ReadingStream` (`mw_station::config`'s module documentation has the general
    /// argument). The reason is specific and it is the supervisor's rather than the
    /// design's: `BuildCx::soft` resolves **once**, in `build`, and
    /// `mw_supervisor`'s `reconcile` never rebuilds a `Running` component when a soft
    /// capability later arrives — the `(State::Running, true)` arm refreshes the
    /// `degraded` label and nothing else.
    ///
    /// `snapshot:latest` has no hard need, so it is built in the **first** reconcile pass,
    /// while `window:rollup` cannot even build until `db:postgres` has published and does
    /// not publish `WindowedView` until its first completed tick. A bus-resolved soft
    /// handle would therefore be `None` for the whole life of every healthy process, and
    /// every snapshot would carry `Staleness::Degraded` forever — the exact inverse of
    /// W3 §2a.6's first table row.
    ///
    /// `RollupState` is already a root resource `main` owns and already implements
    /// `WindowedView`, and its `resolve` answers `UnknownStation` until the first tick —
    /// which [`Folder::resolve`]'s third outcome already handles. So the handle is live
    /// without the bus having to make it so.
    ///
    /// `None` is a node with no rollup at all, which is a node with no weather database.
    /// That is the genuine no-provider case and `Staleness::Degraded` is the right answer
    /// for it.
    fn snapshot_windows(&self) -> Option<&Arc<dyn WindowedView>>;

    /// The payload renderer — M4-2's, see the module documentation. `None` plans neither
    /// component.
    fn snapshot_render(&self) -> Option<&Arc<dyn SnapshotRender>>;
}

/// Declare `snapshot:latest` and `snapshot:warm`.
///
/// Four conditions, each with its own reason and its own log line, because a component that
/// is silently not planned is the defect this programme keeps finding:
///
/// 1. **No renderer** — M4-2 has not landed. Neither component is planned.
/// 2. **No keys** — no websites, or no stations. There is nothing to snapshot.
/// 3. **No reading stream** — no spool, so `OnReading` has no root to validate against.
///    `snapshot:latest` is not planned; `snapshot:warm` still is, and seeds a serve-only
///    node's table once from the database another node wrote.
/// 4. **No weather database** — no `db:postgres`, so `ReadingStore` has no provider.
///    `snapshot:warm` is not planned; `snapshot:latest` still is, because it never queries.
pub fn register<C: SnapshotConfig + ?Sized>(
    cfg: &C,
    out: &mut Vec<ComponentSpec>,
) -> Result<(), SnapshotPlanError> {
    let Some(render) = cfg.snapshot_render() else {
        info!(
            "no snapshot payload renderer is configured, so `snapshot:latest` and \
             `snapshot:warm` are not planned. The renderer is the frozen 133-key DTO's \
             serialiser and `mw-api-legacy` exclusively owns it \
             (implementation-plan.md M4-2)"
        );
        return Ok(());
    };
    let Some(state) = cfg.snapshot_state() else {
        return Err(SnapshotPlanError::NoState);
    };
    if state.is_empty() {
        info!(
            websites = state.websites().len(),
            stations = state.stations().len(),
            "the snapshot key set is empty, so `snapshot:latest` and `snapshot:warm` are \
             not planned -- a key is a (website, station) pair and one of the two sets has \
             no members"
        );
        return Ok(());
    }
    let Some(registry) = cfg.snapshot_registry() else {
        return Err(SnapshotPlanError::NoRegistry);
    };
    let windows = cfg.snapshot_windows().map(Arc::clone);
    if cfg.has_reading_stream() {
        out.push(latest_spec(
            Arc::clone(state),
            Arc::clone(registry),
            Arc::clone(render),
            windows.clone(),
        )?);
    } else {
        info!(
            "this node has no reading stream, so `snapshot:latest` is not planned -- \
             there is nothing to fold. `snapshot:warm` still seeds the table once from the \
             weather database (W7 §8.2's serve-only node)"
        );
    }
    if cfg.has_weather_database() {
        out.push(warm_spec(
            Arc::clone(state),
            Arc::clone(registry),
            Arc::clone(render),
            windows,
        )?);
    } else {
        info!(
            "this node has no weather database, so `snapshot:warm` is not planned -- there \
             is no store to seed from. `snapshot:latest` is unaffected, because it never \
             queries (D-35)"
        );
    }
    Ok(())
}

/// Why a configuration does not produce a snapshot plan.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotPlanError {
    /// `main` did not build the shared state before `plan()` ran.
    #[error(
        "the snapshot table has not been built, so `snapshot:latest` and `snapshot:warm` \
         have nothing to write. `main` builds it beside the spool, the station registry \
         and the window rollup (crates/modernweather/src/boot.rs)."
    )]
    NoState,

    /// `main` did not publish the station registry before `plan()` ran.
    #[error(
        "the station registry has not been published, so the snapshot components have no \
         source for a station's coordinates or snow base distance -- which are \
         configuration and not something a reading carries (W4 assumption 24). \
         `main` builds it in `attach_registry` (crates/modernweather/src/boot.rs)."
    )]
    NoRegistry,

    /// A component id that is not a component id, which would be a change to the constants
    /// above rather than anything configuration can cause.
    #[error("`{id}` cannot name a component")]
    BadId {
        /// Which name.
        id: String,
        /// Why.
        #[source]
        source: ComponentIdError,
    },
}

/// Declare `snapshot:latest`.
pub fn latest_spec(
    state: Arc<SnapshotState>,
    registry: Arc<dyn StationRegistry>,
    render: Arc<dyn SnapshotRender>,
    windows: Option<Arc<dyn WindowedView>>,
) -> Result<ComponentSpec, SnapshotPlanError> {
    let id = ComponentId::new(LATEST_ID).map_err(|source| SnapshotPlanError::BadId {
        id: LATEST_ID.to_string(),
        source,
    })?;
    let digest = ConfigDigest::of(&key_digest_input(&state));
    Ok(ComponentSpec::new(
        id,
        Trigger::OnReading {
            // Every station: a key exists for every (website, station) pair the
            // configuration produces, and filtering here would mean the filter and the key
            // set are two answers to which stations matter.
            filter: ReadingFilter::All,
            // The newest matters and nothing else: a snapshot is a current-conditions
            // payload, so a coalescing `watch` is not a compromise but the semantics.
            policy: ReadingPolicy::Latest,
            deadline: LATEST_DEADLINE,
        },
        Builder::on_reading(move |cx| {
            let state = Arc::clone(&state);
            let registry = Arc::clone(&registry);
            let render = Arc::clone(&render);
            // **Not `cx.soft()`.** `SnapshotConfig::snapshot_windows` has the whole of why;
            // the short version is that a soft handle is resolved once and this component
            // is built before `window:rollup` can possibly have published, so the bus
            // would hand it `None` on every healthy node and never correct it.
            let windows = windows.clone();
            async move {
                let publisher = cx.publisher::<Arc<dyn LatestSnapshot>>()?;
                let rev = PayloadRev::of(render.revision(), cx.cx().generation());
                // **Published in `build`, before the first fold.** D-48's precondition:
                // a consumer whose only hard need is this capability must never be
                // `Waiting`, because a frozen route that is not mounted answers 503 and a
                // frozen route is not allowed to. See the module documentation.
                publisher.publish(Arc::clone(&state) as Arc<dyn LatestSnapshot>);
                info!(
                    keys = state.len(),
                    windows = windows.is_some(),
                    "the latest-conditions snapshot is published with {} keys; it answers \
                     404 for each until a reading arrives",
                    state.len(),
                );
                Ok(Fold {
                    folder: Folder::new(state, registry, render, windows, rev),
                    clock: Arc::new(SystemClock),
                })
            }
        }),
    )
    // **The only need, and it is soft.** `StationRegistry` and `WindowedView` both travel
    // through the registrar (see `SnapshotConfig`'s two accessors) and the declaration
    // stays here, which is `mw_station`'s shape for `ReadingStream`: `plan()` and
    // `mw_component_state` both read the declaration, and the handle's lifetime is the
    // thing the bus cannot give this component. `ReadingStore` is deliberately absent:
    // `api:legacy` is handed this component's capability and never a store handle, so the
    // `/latest` handler *cannot* query (**D-35**) -- and neither can this component, which
    // is the same guarantee one level up.
    .requires([Need::soft(Capability::WindowedView)])
    .provides([Provision::of(Capability::LatestSnapshot)])
    .config_digest(digest))
}

/// Declare `snapshot:warm`.
pub fn warm_spec(
    state: Arc<SnapshotState>,
    registry: Arc<dyn StationRegistry>,
    render: Arc<dyn SnapshotRender>,
    windows: Option<Arc<dyn WindowedView>>,
) -> Result<ComponentSpec, SnapshotPlanError> {
    let id = ComponentId::new(WARM_ID).map_err(|source| SnapshotPlanError::BadId {
        id: WARM_ID.to_string(),
        source,
    })?;
    let digest = ConfigDigest::of(&key_digest_input(&state));
    Ok(ComponentSpec::new(
        id,
        Trigger::OnStartup {
            deadline: WARM_DEADLINE,
        },
        Builder::tick(move |cx| {
            let state = Arc::clone(&state);
            let registry = Arc::clone(&registry);
            let render = Arc::clone(&render);
            // Not `cx.soft()`, for the reason above.
            let windows = windows.clone();
            async move {
                // The upcast to the half this component declared, as `window:rollup` does:
                // after this line the body holds a read handle and cannot store anything.
                let store: Arc<dyn ReadingStore> = cx.hard::<Arc<dyn Store>>()?;
                let rev = PayloadRev::of(render.revision(), cx.cx().generation());
                Ok(Warm {
                    store,
                    folder: Folder::new(state, registry, render, windows, rev),
                    clock: Arc::new(SystemClock),
                })
            }
        }),
    )
    // **Hard, where D-37 writes soft.** The module documentation has the whole of why; the
    // short version is that a soft need delays nothing, so a soft `ReadingStore` would seed
    // nothing on every node including a healthy one.
    .requires([
        Need::hard(Capability::ReadingStore),
        Need::soft(Capability::WindowedView),
    ])
    .config_digest(digest))
}

/// What goes into both specs' [`ConfigDigest`].
///
/// **The key set *and the contents of every `WebsiteView`*.** The key set alone is not what
/// either component closes over, which is the mistake this function made first: [`Folder`]
/// holds `state.websites()` and puts the whole view — name, primary station, snow binding —
/// into every [`SnapshotSource`], where the renderer reads it.
///
/// With only the keys hashed, an operator who renamed a website or repointed its
/// `snow_station_key` would leave the key set unchanged, so the digest unchanged, so the
/// component not restarted, so the stale view still rendering — while
/// [`mw_component::PayloadRev`] *does* move with the configuration generation. The `ETag`
/// would change and the body would not, which is worse than not invalidating at all and is
/// the opposite of what `payload_rev` exists for.
///
/// W3 §2a.4 says entries are "created and destroyed by the config watch's plan diff", and
/// that only holds if the diff can see a content change. Config reload is M7-1's, so this
/// is latent — and the digest is the only mechanism, so it is cheaper to be right here than
/// to debug there.
fn key_digest_input(state: &SnapshotState) -> String {
    let mut s = String::new();
    for website in state.websites() {
        let _ = write!(
            s,
            "w{}:{}:{}:{}:{};",
            website.id,
            website.name,
            website.primary_station.as_ref().map_or("-", |k| k.as_str()),
            website.snow_enabled,
            website.snow_station.as_ref().map_or("-", |k| k.as_str()),
        );
    }
    for station in state.stations() {
        s.push_str(station.as_str());
        s.push(';');
    }
    s
}

/// The rendering half both components share.
///
/// Not a trait and not a component: it is the arithmetic between "here is a reading" and
/// "here are the bytes", and both triggers need all of it. Keeping it one type is what makes
/// a seeded snapshot and a folded one the same object — a warm start that produced a
/// slightly different payload from the first reading's would be a visible jump on the
/// client.
struct Folder {
    state: Arc<SnapshotState>,
    registry: Arc<dyn StationRegistry>,
    render: Arc<dyn SnapshotRender>,
    /// `None` when `window:rollup` had not published at build — a node whose database has
    /// never been up. The cold-start case, not the degraded one.
    windows: Option<Arc<dyn WindowedView>>,
    rev: PayloadRev,
}

impl std::fmt::Debug for Folder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Folder")
            .field("keys", &self.state.len())
            .field("windows", &self.windows.is_some())
            .field("payload_rev", &self.rev)
            .finish_non_exhaustive()
    }
}

impl Folder {
    fn new(
        state: Arc<SnapshotState>,
        registry: Arc<dyn StationRegistry>,
        render: Arc<dyn SnapshotRender>,
        windows: Option<Arc<dyn WindowedView>>,
        rev: PayloadRev,
    ) -> Self {
        Self {
            state,
            registry,
            render,
            windows,
            rev,
        }
    }

    /// Render one station's reading into every configured website's key.
    ///
    /// One resolve of the window declarations per station rather than per key, because the
    /// windows are a property of the station and the websites differ only in what they bind
    /// — so six websites bound to one device are six payloads over one resolution.
    fn fold(
        &self,
        reading: &Reading,
        station: &StationKey,
        now: DateTime<Utc>,
        seed: bool,
    ) -> Result<usize, ComponentError> {
        let (windows, staleness) = self.resolve(station, now);
        let meta = self.registry.metadata(station);
        let mut written = 0usize;
        for website in self.state.websites() {
            let source = SnapshotSource {
                key: SnapshotKey::new(website.id, station.clone()),
                website: website.clone(),
                reading: reading.clone(),
                meta,
                windows: windows.clone(),
            };
            // **A render failure is a per-key, counted non-event — not a tick failure.**
            //
            // Returning `Err` here is the obvious thing and it would break the invariant
            // this item exists to establish. The chain: `ComponentError` out of `readings`
            // -> `Exit::Failed` -> `mw_supervisor`'s `on_exit` calls `bus.revoke_all(id)`
            // -> `LatestSnapshot` is unavailable -> `api:legacy`'s hard need is missing ->
            // `stop_component` + `Waiting` -> its route table is unmounted -> **a frozen
            // path answers `503`** for the length of the backoff. Publishing in `build`
            // makes the capability available once; it does not make it un-revokable, and
            // **D-48** forbids the outcome however it is reached.
            //
            // So the previous bytes stay served, the failure is counted and logged with its
            // class, and the batch is acknowledged. A payload that cannot be re-rendered
            // serving its last good value is also strictly closer to **D-36** than
            // unmounting `/latest`.
            //
            // The second reason, independent of D-48: a mid-loop `?` would leave the table
            // **partially** updated — website 1's key advanced and website 2's did not — so
            // two websites would serve different readings for one station.
            let snapshot = match Snapshot::render(
                source,
                staleness.clone(),
                now,
                self.rev,
                Arc::clone(&self.render),
            ) {
                Ok(snapshot) => Arc::new(snapshot),
                Err(e) => {
                    warn!(
                        website = %website.id,
                        %station,
                        class = ?mw_types::Classify::class(&e),
                        error = %mw_types::DisplayChain(&e),
                        "this website's current-conditions payload could not be \
                         serialised, so its previous bytes stay published; the reading \
                         itself is durable and unaffected"
                    );
                    m::snapshot_render_failed(&SnapshotKey::new(website.id, station.clone()));
                    continue;
                }
            };
            let key = snapshot.key().clone();
            let stored = if seed {
                self.state.seed(snapshot)
            } else {
                self.state.publish(snapshot)
            };
            if stored {
                written += 1;
                m::snapshot_published(&key, reading.time, now, staleness.is_fresh());
            }
        }
        Ok(written)
    }

    /// The window-derived values, and what their age says about **D-36**'s staleness.
    ///
    /// Three outcomes, and the point of the function is that they are three rather than two:
    ///
    /// | The provider | `windows` | `staleness` |
    /// |---|---|---|
    /// | published and refreshing | its values | `Fresh` |
    /// | published and its `as_of` is older than [`WINDOW_STALE_AFTER`] | its **last good** values | `Degraded` |
    /// | never published | empty | `Degraded` |
    ///
    /// The middle row is the one **D-36** is about, and the mechanism is the one W2a
    /// Decision 21 gives for free: a soft handle is resolved once in `build`, so when
    /// `window:rollup` returns to `Waiting { ReadingStore }` this component keeps the
    /// `Arc` it already has and keeps reading the last state that component published.
    /// There is no code here that notices the outage — the values simply stop moving, and
    /// `Resolved::as_of` is what makes that observable rather than invisible.
    fn resolve(&self, station: &StationKey, now: DateTime<Utc>) -> (Resolved, Staleness) {
        let missing = || SmallVec::from_slice(&[Capability::WindowedView]);
        let Some(view) = &self.windows else {
            return (
                Resolved::new(now, []),
                Staleness::Degraded {
                    missing: missing(),
                    // The provider has never answered, so there is no instant it stopped
                    // at. `now` is the honest `since` for "as far back as this process
                    // knows", and the log line above says which case it is.
                    since: now,
                },
            );
        };
        match view.resolve(station, &WINDOWS) {
            Ok(resolved) => {
                let age = now.signed_duration_since(resolved.as_of());
                if age > STALE_AFTER {
                    let since = resolved.as_of();
                    (
                        resolved,
                        Staleness::Degraded {
                            missing: missing(),
                            since,
                        },
                    )
                } else {
                    (resolved, Staleness::Fresh)
                }
            }
            Err(e) => {
                // A declaration or configuration failure, which is `Fatal` and the same
                // answer on the next reading (`WindowError`'s own classification). Not a
                // reason to refuse the payload: every window-derived field is `None` and
                // the rest of the snapshot — the reading itself, the enrich stage's
                // derived columns, the sun and moon — is unaffected, which is exactly
                // W3 §2a.6's first table row.
                //
                // `UnknownStation` is the expected member here, and it has **two**
                // meanings rather than one. Its own documentation names both: a station
                // this node does not collect for — a peer forwards its readings, so it is
                // inventory rather than a component (`mw_station::config`) — **and** a
                // station whose provider has not completed a tick yet. The second is
                // routinely transient for up to a tick period after a restart, so "the
                // same answer on the next reading" is true of the enum's classification
                // and not of this variant.
                //
                // Logged at `warn` once per fold, which the dedupe layer's token bucket
                // does collapse: the event carries a `station` field, which is `mw_obs`'s
                // dedupe key, and no `class`, so it is not in the never-suppress set.
                // Three, then a summary per minute.
                warn!(
                    %station,
                    error = %mw_types::DisplayChain(&e),
                    "the window-derived values for this station could not be resolved, so \
                     they are absent from its current-conditions payload; everything the \
                     reading itself carries is unaffected"
                );
                (
                    Resolved::new(now, WINDOWS.map(|d| (d, None))),
                    Staleness::Degraded {
                        missing: missing(),
                        // The provider answered, so nothing has a last-good instant to
                        // date from: the declaration is wrong rather than the data stale.
                        since: now,
                    },
                )
            }
        }
    }
}

/// `snapshot:latest`'s body.
struct Fold {
    folder: Folder,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Fold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fold")
            .field("folder", &self.folder)
            .finish_non_exhaustive()
    }
}

impl OnReading for Fold {
    fn readings<'a>(
        &'a mut self,
        batch: ReadingBatch,
        _cx: &'a Cx,
    ) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            let now = self.clock.now();
            // **The newest envelope per station**, not the newest envelope. A batch can
            // carry several stations — the fan-out is one stream for the whole node — and
            // `ReadingPolicy::Latest` coalesces *the stream*, not the batch. Taking
            // `batch.newest()` alone would make a two-station node publish one station's
            // conditions and silently drop the other's on every batch that carried both.
            //
            // A `BTreeMap` rather than a sort: the batch is in arrival order, so the last
            // entry per key is the newest, and `insert` keeps it.
            let mut newest: std::collections::BTreeMap<&StationKey, &Reading> =
                std::collections::BTreeMap::new();
            for envelope in batch.envelopes() {
                newest.insert(&envelope.station, envelope.reading.as_ref());
            }
            for (station, reading) in newest {
                self.folder.fold(reading, station, now, false)?;
            }
            Ok(())
        })
    }
}

/// `snapshot:warm`'s body.
struct Warm {
    store: Arc<dyn ReadingStore>,
    folder: Folder,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Warm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Warm")
            .field("folder", &self.folder)
            .finish_non_exhaustive()
    }
}

impl Tick for Warm {
    fn tick<'a>(&'a mut self, _cx: &'a Cx) -> BoxFuture<'a, Result<(), ComponentError>> {
        Box::pin(async move {
            let now = self.clock.now();
            let since = now - chrono::Duration::seconds(RECENCY.as_secs() as i64);
            let mut seeded = 0usize;
            let mut found = 0usize;
            let stations: Vec<StationKey> = self.folder.state.stations().to_vec();
            for station in &stations {
                let reading = self
                    .store
                    .latest(LatestQuery {
                        station: station.clone(),
                        since,
                    })
                    .await
                    .map_err(|e| ComponentError::tick("read the newest reading", e))?;
                let Some(reading) = reading else {
                    continue;
                };
                found += 1;
                seeded += self.folder.fold(&reading, station, now, true)?;
            }
            info!(
                stations = stations.len(),
                found,
                seeded,
                "the snapshot table is warm: {found} of {} stations had a reading inside \
                 the last {}s, which seeded {seeded} keys. A station with none answers 404 \
                 until it reports",
                stations.len(),
                RECENCY.as_secs(),
            );
            Ok(())
        })
    }
}
