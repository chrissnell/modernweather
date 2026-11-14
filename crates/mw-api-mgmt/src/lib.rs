//! **The management API's server half**: the operations W5 §1 maps onto `ConfigAdmin` +
//! `ComponentControl`, and the OpenAPI schema derived from them.
//!
//! The schema is **derived from these handlers** via `utoipa` rather than hand-maintained,
//! and its own drift gate (`mgmt-schema-drift`) checks the committed schema and TypeScript
//! client against them -- separate from `codegen-drift` on purpose, because that check should
//! keep meaning precisely "the generated output matches `metrics/registry.toml`" and nothing
//! else (DESIGN.md **D-57**).
//!
//! Forbidden to depend on `mw-store` directly -- it writes through `ConfigAdmin` -- or on
//! `mw-pipeline`. Designed by W5.
//!
//! # The five things this crate is, in one sentence each
//!
//! | | |
//! |---|---|
//! | [`routes`] | the handler set, the router, and [`routes::ApiDoc`], which **is** the schema |
//! | [`auth`] | W5 §3 as code: the hashes, the policy, the encodings and [`auth::AuthStore`] |
//! | [`guard`] | what happens to a request before a handler sees it, in the order it happens |
//! | [`admin`] | `ConfigAdmin`: the trait this crate needs from the binary, and the preview/save pair that is one code path |
//! | [`display`] | **D-36** as code: the one place the component model's vocabulary is translated, on the server |
//!
//! # Why `ConfigAdmin` is declared here and implemented in the binary
//!
//! A committed write needs SQL, `plan()` and the configuration *type*, and those live in
//! three crates that `ci/layers.toml` forbids from naming each other -- so the one place all
//! three may be named is the binary (docs/service-architecture.md §1.3 rule 1).
//! [`admin::ConfigAdmin`] is therefore the trait a consumer declares, exactly as
//! `mw_http::ListenerConfig` and `mw_station::StationConfig` are. [`host::LogSource`] and
//! [`host::Machine`] are the same shape for the same reason, and they are what keeps this
//! crate's dependency set to the two capabilities implementation-plan.md **M7-1** names.
//!
//! # What makes the save preview worth having
//!
//! [`admin::ConfigAdmin::apply`] takes a [`admin::Commit`], and that flag is the **only**
//! difference between `POST /api/v1/config/preview` and a `PUT`: both open a transaction,
//! apply the change, read the configuration back out of that transaction, run the
//! supervisor's pure `plan()` on it and diff against the live plan. The preview rolls back
//! and the save commits. So "a save's preview names exactly the components that then
//! restart" -- M7-1's done-criterion -- is a property of there being one code path rather
//! than an agreement two of them have to keep.
//!
//! # The surface is authenticated, and the loopback narrowing now means what it says
//!
//! M7-1 shipped this crate with no authentication at all and an unconditional narrowing of
//! the management bind address to loopback to make that safe to ship. **M7-2 replaces both
//! halves of that.** [`auth`] is W5 §3 -- Argon2id at m=64 MiB/t=3/p=4, 32-byte session
//! secrets stored as SHA-256, the single-use setup code -- and [`guard`] is the gate in front
//! of every one of M7-1's forty operations. The narrowing is now conditional on the instance
//! being **unclaimed** (W5 §3.4), with `--setup-listen` as the headless escape hatch, which
//! is the binary's to apply because the binary is what binds.
//!
//! # What is here already, and why one module arrived before the others
//!
//! [`freshness`] renders W2a **Decision 20**'s data-presence declaration -- "Forecast last
//! updated 3 hours ago." / "Forecast has never been fetched." -- **separately from
//! component state**, which is `docs/management-ui-design.md` §0.4's rule and the second
//! half of implementation-plan.md **M6-3**'s done-criterion. It is here rather than in
//! `mw-component` because the rule it implements is **D-36**: the component model's
//! vocabulary must not reach a screen, and W5 §0 owns the translation. This crate is W5's
//! half.
//!
//! It is **not yet wired into [`runtime::ComponentView`]**, and that is stated rather than
//! implied: a freshness sentence needs the instant its capability was last written, which no
//! `ComponentStatus` carries. [`runtime`]'s own documentation has the full note, and §4.4's
//! "every resource is an object with named fields" is what makes adding the field later
//! additive.

/// `ConfigAdmin`, the preview/save pair, and the plan outline a change answers with.
pub mod admin;
/// W5 §3 as code: Argon2id, the session, the setup claim, and `AuthStore` (**M7-2**).
pub mod auth;
/// What a write sends, and the validation that answers in the operator's words (§7.2, §8.5).
pub mod change;
/// **D-36** as code: §0.2's table and §0.4's state, trigger and counter rendering.
pub mod display;
/// One error body, with field paths rather than a banner (§8.5).
pub mod error;
/// W2a Decision 20's data presence, rendered separately from component state (§0.4).
pub mod freshness;
/// The CSRF pair, the CORS allowlist, the security headers and the session gate (**M7-2**).
pub mod guard;
/// The log rings and the machine, as traits rather than dependencies.
pub mod host;
/// The handler set, the router, and the schema derived from it.
pub mod routes;
/// §2.3's runtime status resource and §2.5's root-cause rollup.
pub mod runtime;
/// What a read of the configuration returns, with every secret **omitted** (**D-67**).
pub mod view;

pub use admin::{Applied, Commit, ConfigAdmin, PlanOutline};
pub use auth::{AuthStore, Minted, NewAccount, NewSession, SetupCode, StoredCredential};
pub use change::Change;
pub use display::{Display, Section, Vocabulary};
pub use error::{ApiError, Body, ErrorBody, FieldError};
pub use freshness::{render, FreshnessView, Subject};
pub use guard::{Current, Guard, Peer};
pub use host::{
    LogEntry, LogField, LogSource, LogsView, Machine, SerialPort, SerialPortsView, Stream,
};
pub use routes::{router, ApiDoc, Mgmt};
pub use view::{ConfigView, Credential, Named};

/// The management schema, as the JSON `cargo xtask mgmt-schema` commits.
///
/// Here rather than in `xtask` so that the emitter and the handlers cannot be built against
/// different versions of each other: `xtask` calls this, which means the bytes the gate
/// compares were produced by the same compilation as the routes they describe.
///
/// **Deterministic**, which a drift gate requires: `utoipa`'s default (no `preserve_order`)
/// keys its paths and schemas by `BTreeMap`, so the output is sorted rather than
/// insertion-ordered, and two runs of the same tree produce the same bytes.
pub fn openapi_json() -> Result<String, serde_json::Error> {
    use utoipa::OpenApi as _;
    let mut json = ApiDoc::openapi().to_pretty_json()?;
    // A trailing newline, because every other file the repository commits has one and a
    // diff on the last line is noise in every review that touches it.
    json.push('\n');
    Ok(json)
}

#[cfg(test)]
mod tests {
    /// The schema is derived from the handlers, so the assertion worth making about the
    /// emitter is that it produces the **same bytes twice** -- which is what the drift gate
    /// rests on and the one way `utoipa` could quietly break it (by growing an
    /// insertion-ordered map).
    #[test]
    fn the_schema_is_byte_identical_across_runs() {
        let a = super::openapi_json().expect("the schema serialises");
        let b = super::openapi_json().expect("the schema serialises");
        assert_eq!(a, b);
        assert!(a.ends_with("}\n"), "the schema ends with a newline");
    }

    /// Every path is `/api/v1/*` (**D-49**), and the frozen public contract is not described
    /// here at all (**D-37**): a management schema that documented `/latest` would invite a
    /// client to treat the frozen surface as versioned.
    #[test]
    fn every_path_is_under_api_v1() {
        use utoipa::OpenApi as _;
        let doc = super::ApiDoc::openapi();
        assert!(!doc.paths.paths.is_empty(), "the schema has operations");
        for path in doc.paths.paths.keys() {
            assert!(
                path.starts_with("/api/v1/"),
                "`{path}` is not under /api/v1/"
            );
        }
    }
}
