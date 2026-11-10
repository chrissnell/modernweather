//! The public website: `askama` templates, embedded assets, the inline configuration block
//! and `/config.json`.
//!
//! Templates are compile-time checked, so a missing field is a build error rather than a
//! request-time one (A-W3-1). No template under `templates/` may use the `safe` filter or
//! `escape("none")` -- `cargo xtask gate layout` fails the build on either (**D-81**),
//! because an escaping opt-out is the one way a compile-time checked template can still
//! emit unescaped output. The same gate pins `askama.toml`'s escaper to
//! [`GoHtmlEscaper`], because reassigning the `html` escaper would turn escaping off
//! globally with no filter appearing anywhere.
//!
//! Forbidden to depend on `mw-store`, `mw-proto` or `mw-api-legacy`.
//!
//! # What this crate serves
//!
//! | Path | Scope | Answer |
//! |---|---|---|
//! | `/` | per website | the weather page, or the portal page for a portal website |
//! | `/portal` | per portal website | the portal page; a non-portal website does not declare it, so it 404s |
//! | `/config.json` | per website | **additive** (**D-79**): the same nine values the inline block carries |
//! | `/privacy`, `/support` | every host | the two static pages, which bypass the website check (§6.3) |
//! | everything else | every host, **catch-all** | the embedded asset tree, or `404 page not found\n` |
//!
//! # The three decisions a reader should find here rather than infer
//!
//! **1. The acceptance test is a byte diff against W1's recorded bodies**, not a substring
//! check. `tests/byte_diff.rs` reads `contract/fixtures/html-weather-station.json` and
//! `html-portal.json` and compares every byte, with the one expected delta applied as a
//! patch. That is what [`GoHtmlEscaper`] buys, and it is also what found the two things
//! §7.1's single-escaper account leaves out -- the `+` → `&#43;` entry and the JavaScript
//! context inside `<script type="application/json">`. [`escape`] and [`gojs`] have both.
//!
//! **2. Nothing in this crate is marked `HtmlSafe` except a number.** [`gojs::JsNumber`] is
//! the one type, and the reason is an exponent's `+` rather than convenience; there is no
//! string path through it. `AboutStationHTML` is not rendered into any page at all --
//! §7.5's premise correction -- so the one value that would want raw HTML does not reach a
//! template, and the gate forbids the filter that would let it.
//!
//! **3. The mime table is a hard-coded `const`** (**D-76**), asserted against a capture of
//! Go's own file server. [`assets`] has the two quirks and why a mime crate gets them
//! "right".

pub mod assets;
pub mod component;
pub mod config;
pub mod escape;
pub mod gojs;
pub mod page;

pub use component::{
    register, site_table, static_table, tables, SiteState, WebConfig, WebPlanError, SITE_PREFIX,
    STATIC_ID,
};
pub use config::{config_json, SiteView};
pub use escape::GoHtmlEscaper;
pub use page::{weather_station, RenderError};
