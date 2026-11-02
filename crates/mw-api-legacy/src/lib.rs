//! **The compatibility adapter**, and the only place the frozen contract's oddities live.
//!
//! The 133-key `LegacyWeatherReading` and its exact `serde` renames including the
//! inconsistent ones; the duplications (`ohum`/`outhumidity`, `dayrain`/`rainday`) emitted
//! from one value; the unit split (`snowdepth` in millimetres on `/latest`, inches on
//! `/snow` and `/span`); the `omitempty` semantics; the four error-body conventions and
//! their exact bytes; and `format=msgpack` (docs/service-architecture.md §1.7).
//!
//! **The zero-fill is exactly one function**, `zf`, used by the DTO constructors and
//! nowhere else -- the layout gate asserts that `unwrap_or(0.0)` appears nowhere else in
//! the workspace. The domain type is `Option<f32>` end to end and the database stores
//! `NULL`; the contract's zeros are produced here, at the DTO, one layer above storage.
//!
//! It is handed `LatestSnapshot` and the read stores as capability handles, so it may not
//! depend on `sqlx` or `mw-store`: the `/latest` handler **cannot** query (W2a D-35).
