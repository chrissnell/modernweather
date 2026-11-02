//! The management API's server half: 46 routes over `ConfigAdmin` + `ComponentControl`.
//!
//! The OpenAPI schema is **derived from these handlers** via `utoipa` rather than
//! hand-maintained, and its own drift gate (`mgmt-schema-drift`) checks the committed
//! schema and TypeScript client against them -- separate from `codegen-drift` on purpose,
//! because that check should keep meaning precisely "the generated output matches
//! metrics/registry.toml" and nothing else (DESIGN.md D-57).
//!
//! Admin credentials are Argon2id PHC strings compared in constant time. Forbidden to
//! depend on `mw-store` directly -- it writes through `ConfigAdmin` -- or on `mw-pipeline`.
//! Designed by W5; a stub here.
