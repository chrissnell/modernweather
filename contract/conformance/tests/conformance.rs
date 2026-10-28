//! `cargo test` wrapper so CI can run the suite as a test rather than a binary.
//! Skips itself when CONFORMANCE_BASE_URL is unset, so `cargo test` stays green
//! in a checkout with no server running.

use legacy_conformance::{load_fixtures, run_all};
use std::path::PathBuf;

#[test]
fn legacy_contract_is_satisfied() {
    let Ok(base) = std::env::var("CONFORMANCE_BASE_URL") else {
        eprintln!("CONFORMANCE_BASE_URL is not set; skipping the legacy conformance suite");
        return;
    };
    let dir = PathBuf::from(
        std::env::var("CONFORMANCE_FIXTURES").unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("fixtures").display().to_string()
        }),
    );
    let fixtures = load_fixtures(&dir).expect("fixtures directory is readable");
    assert!(!fixtures.is_empty(), "no fixtures found in {}", dir.display());
    let report = run_all(&base, &fixtures);
    print!("{}", report.text);
    assert_eq!(report.failed, 0, "{} fixtures failed against {base}", report.failed);
}
