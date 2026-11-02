//! The collector: W7's uplink and gRPC receiver.
//!
//! In the Go version, sending readings to a peer is implemented as a *storage engine* and
//! receiving them is implemented as a *weather station*, and the role that actually exists
//! has no name anywhere in the codebase. It has one here (DESIGN.md D-31).
//!
//! Forbidden to depend on `mw-api-legacy` or `mw-web`.
