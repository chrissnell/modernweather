//! Stations: `Transport`, `Session`, `StationIdentity`, identity resolution, and the
//! station components.
//!
//! The four-part decomposition of W2a §4a: a transport moves bytes, a session owns the
//! conversation, a codec (in `mw-codec`, which cannot see a socket) turns a frame into
//! readings, and identity resolution says which station they belong to. Forbidden to
//! depend on `mw-store`, `mw-api-legacy` or `mw-web`.
