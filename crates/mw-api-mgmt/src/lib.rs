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
//! Designed by W5; a stub here, with one exception.
//!
//! # What is here already, and why one module arrived before the other forty-six
//!
//! [`freshness`] renders W2a **Decision 20**'s data-presence declaration -- "Forecast last
//! updated 3 hours ago." / "Forecast has never been fetched." -- **separately from
//! component state**, which is `docs/management-ui-design.md` §0.4's rule and the second
//! half of implementation-plan.md **M6-3**'s done-criterion. It is here rather than in
//! `mw-component` because the rule it implements is **D-36**: the component model's
//! vocabulary must not reach a screen, and W5 §0 owns the translation. This crate is W5's
//! half.
//!
//! The routes, the schema and the authentication are M7-1's and M7-2's. This module needs
//! none of them: it is a pure function from a declaration and an instant to a sentence,
//! which is what lets the item that *produces* the declarations also prove they render.

pub mod freshness;

pub use freshness::{render, FreshnessView, Subject};
