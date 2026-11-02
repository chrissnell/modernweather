//! The public website: `askama` templates, embedded assets, the inline configuration
//! block and `/config.json`.
//!
//! Templates are compile-time checked, so a missing field is a build error rather than a
//! request-time one (A-W3-1). No template under `templates/` may use the `safe` filter or
//! `escape("none")` -- `cargo xtask gate layout` fails the build on either (W3 decision
//! 81), because an escaping opt-out is the one way a compile-time checked template can
//! still emit unescaped output.
//!
//! Forbidden to depend on `mw-store`, `mw-proto` or `mw-api-legacy`.
