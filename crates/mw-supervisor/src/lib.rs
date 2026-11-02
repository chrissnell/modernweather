//! The supervisor: `plan()`, the hard-edge digraph, one timer heap, the task table,
//! capability watches, the `Spool` handle and the fan-out.
//!
//! It does not know its components (docs/service-architecture.md §1.3, rule 2). Each
//! component crate contributes one `fn register(cfg, out)` and the binary assembles the
//! list, so adding a component crate never edits this one and no `match` arm per
//! component kind can grow here -- which is the shape `managers/controller.go` took, down
//! to its `if con.Type == "management" { continue }`.
//!
//! One timer heap replaces 21 hand-rolled clocks, and `tokio::spawn` is reachable only
//! from this crate's `task` and `clock` modules (§1.4, §3.2).
