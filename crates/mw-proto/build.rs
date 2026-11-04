//! Compile the two protobuf schemas into `OUT_DIR`.
//!
//! **`protox`, not `protoc`** (A-W3-1). `protox` is a pure-Rust parser, so no `protoc`
//! binary is needed on any of the four cross targets in `ci/release.toml` or on a Pi. A
//! build-time dependency on an external binary is the thing **D-43**'s static-musl
//! linkage exists to avoid more of.
//!
//! **Into `OUT_DIR`, not into `generated/`** (docs/service-architecture.md §1.6). This is
//! the one exception to "generated output is committed", and the reason is that it would
//! carry no information: `generated/proto/reading.proto` is itself drift-gated by
//! `cargo xtask codegen --check`, so the prost output is a pure function of an input that
//! is already gated and a second check would gate the same fact twice.
//!
//! This file is the `only-mw-types-and-the-proto-build-script-mention-generated` exemption
//! in `ci/layers.toml` being used for the first time. That rule stays **crate-wide** for
//! `mw-proto` rather than narrowing to this path, and the reason is written out beside the
//! rule: the gate's scan cannot see comments, so narrowing it would fail on the four doc
//! comments in this crate that name the schema in prose. The rule that enforces the
//! decision — a hand-written module may not reach a generated *type* — is
//! `only-mw-types-adopts-generated-rust`, which matches on the generated **Rust**
//! directory and whose `only` list does not include this crate. (Spelling that path here
//! would itself trip that rule, which is the same blindness-to-comments the note beside it
//! describes.)

use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    // `crates/mw-proto` -> the workspace root. Two components, asserted rather than
    // assumed: a crate that moves and silently reads a schema from the wrong tree would
    // compile and be wrong.
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or("crates/mw-proto is not two levels below the workspace root")?
        .to_path_buf();

    let registry_protos = root.join("generated").join("proto");
    let own_protos = manifest.join("proto");
    let reading = registry_protos.join("reading.proto");
    let collector = own_protos.join("collector.proto");

    for p in [&reading, &collector] {
        if !p.exists() {
            return Err(format!("{} does not exist", p.display()).into());
        }
        println!("cargo::rerun-if-changed={}", p.display());
    }

    // Both include paths, so `collector.proto`'s `import "reading.proto"` resolves against
    // the generated tree without naming it. The well-known types come from `protox`'s own
    // bundled copies.
    let descriptors = protox::compile([&reading, &collector], [&registry_protos, &own_protos])?;

    let mut config = prost_build::Config::new();
    config
        // The domain type is `Option`-valued end to end and the wire type is what carries
        // that across a hop and a restart, so `Option<f32>` on the Rust side of an
        // `optional float` is the whole point. prost does this for `optional` already;
        // naming the bytes type is the one place a default would cost a copy per field.
        .bytes([
            ".collector.v1.Envelope.origin",
            ".collector.v1.Envelope.path",
        ])
        .compile_fds(descriptors)?;
    Ok(())
}
