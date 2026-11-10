//! **The compatibility adapter**, and the only place the frozen contract's oddities live.
//!
//! The 133-key `LegacyWeatherReading` and its exact `serde` renames including the
//! inconsistent ones; the duplications (`ohum`/`outhumidity`, `dayrain`/`rainday`) emitted
//! from one value; the unit split (`snowdepth` in millimetres on `/latest`, inches on
//! `/snow` and `/span`); the `omitempty` semantics; and the four error-body conventions
//! and their exact bytes (docs/service-architecture.md §1.7).
//!
//! **The zero-fill is exactly one function**, [`dto::zf`], used by the DTO constructors and
//! nowhere else -- the layout gate asserts that `unwrap_or(0.0)` appears nowhere else in
//! the workspace. The domain type is `Option<f32>` end to end and the database stores
//! `NULL`; the contract's zeros are produced here, at the DTO, one layer above storage.
//!
//! It is handed [`mw_component::LatestSnapshot`] and the read stores as capability
//! handles, so it may not depend on `sqlx` or `mw-store`: the `/latest` handler **cannot**
//! query (W2a D-35).
//!
//! # What is here, and what is deliberately not
//!
//! | Endpoint | Class (`docs/reconciliation.md` §6.4) | Where |
//! |---|---|---|
//! | `/latest` | frozen | [`render`] renders the body; [`latest`] serves it from the snapshot |
//! | `/span/{span}` | frozen | [`span`] |
//! | `/snow` | frozen | [`snow`] |
//! | `/stationinfo` | frozen | [`stationinfo`] |
//! | `/forecast/{span}` | frozen | [`forecast`] |
//! | `/almanac` | coupled | [`almanac`] |
//! | `/api/stations` | coupled | [`stations`] |
//! | the HTML pages and the four asset trees | coupled | **`mw-web`, implementation-plan.md M4-3** |
//! | `/alerts`, `/api/remote-stations`, `format=msgpack` | retired | **nowhere, by D-69** |
//!
//! `format=msgpack` is why `rmp-serde` is *not* a dependency of this crate: **D-69** does
//! not carry it forward, the `Encoding::MsgPack` cell on a [`mw_component::Snapshot`] is
//! never filled, and [`render::Renderer`] declares `[Encoding::Json]` so that asking for
//! anything else is a named refusal rather than an empty body.
//!
//! # Where the routes are mounted, and why that is not here either
//!
//! The router, host routing and `RouteTable` mounting are `mw-http`'s, which M4-3 built.
//! What this crate exposes is [`route_table`] -- the `axum::Router` over a [`RouteState`]
//! **and** [`manifest`], the pure declaration **D-51** puts on the component's `Provision`,
//! as one `mw_http::RouteTable` so that the paths `plan()` validated and the paths the
//! router answers cannot be assembled from different configurations. The binary builds it
//! once and hands it to the one listener; the component installs its handler set into the
//! state while it runs and clears it on the way out, so "the table is mounted" and "the
//! component is `Running`" are the same fact.
//!
//! **D-48 is why that matters and why it cannot be violated here.** `api:legacy`'s only
//! hard need is `LatestSnapshot`, whose provider has no hard need of its own, so the
//! component is built in the SECOND reconcile pass -- `snapshot:latest` publishes in its own
//! build, which is the first -- and the table is never unmounted on a
//! healthy process -- with the database down, every frozen route still answers its contract
//! response. The three read stores are `Soft` and resolved through
//! [`mw_component::Deferred`], which is what makes "degrade per request" real rather than a
//! handle that was `None` at build and stays that way.

/// `/almanac` -- coupled class.
pub mod almanac;
/// The component: `api:legacy`'s spec, its `Serve` body and the state slot.
pub mod component;
/// What this crate plans from: the website and station rows, and the listener.
pub mod config;
/// The 133-key DTO, the zero-fill and Go's number format.
pub mod dto;
/// `/forecast/{span}` -- the `jsonb` splice (**D-39**).
pub mod forecast;
/// `/latest` -- four things and no queries (**D-34**, **D-35**).
pub mod latest;
/// The query string, parsed the way Go's `url.Values.Get` reads it.
pub mod params;
/// `SnapshotRender`: the `/latest` body, rendered once per reading version (**D-33**).
pub mod render;
/// The frozen response conventions: headers, the four error bodies, the additive headers.
pub mod reply;
/// `/snow` -- the website's gauge, and the depth the request may redirect (Q-8).
pub mod snow;
/// `/span/{span}` -- the materialised prefix, the trailing bucket and the 60-second cache
/// (**D-54**, **D-55**).
pub mod span;
/// `/stationinfo` -- and **DEV-10**, the one signed deviation on the frozen surface.
pub mod stationinfo;
/// `/api/stations` -- coupled class.
pub mod stations;
/// Sunrise, sunset and the four lunar values: functions of `(date, lat, lon)` and not of a
/// reading (W2a Decision 36).
pub mod sun;

pub use component::{register, Handlers, LegacyPlanError, RouteState, ID};
pub use config::{LegacyConfig, Listener, StationView, WebsiteView};
pub use dto::zf;
pub use render::Renderer;
pub use span::SpanCache;

pub use component::{manifest, route_table, routes};
