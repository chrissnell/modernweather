use legacy_conformance::{load_fixtures, run_all};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut base = std::env::var("CONFORMANCE_BASE_URL").unwrap_or_default();
    let mut dir = PathBuf::from(
        std::env::var("CONFORMANCE_FIXTURES")
            .unwrap_or_else(|_| default_fixture_dir().display().to_string()),
    );
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--base-url" => base = args.next().unwrap_or_default(),
            "--fixtures" => dir = PathBuf::from(args.next().unwrap_or_default()),
            "-h" | "--help" => {
                eprintln!(
                    "legacy-conformance --base-url <URL> [--fixtures <DIR>]\n\n\
                     Replays the frozen legacy REST fixtures against <URL> and asserts JSON\n\
                     compatibility. Point it at the Go daemon to prove the fixtures describe\n\
                     reality; point it at the Rust daemon to prove the rewrite matches.\n\n\
                     Environment: CONFORMANCE_BASE_URL, CONFORMANCE_FIXTURES."
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }
    if base.is_empty() {
        eprintln!("error: --base-url (or CONFORMANCE_BASE_URL) is required");
        return ExitCode::from(2);
    }
    let fixtures = match load_fixtures(&dir) {
        Ok(f) if !f.is_empty() => f,
        Ok(_) => {
            eprintln!("error: no fixtures found in {}", dir.display());
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", dir.display());
            return ExitCode::from(2);
        }
    };
    eprintln!("replaying {} fixtures from {} against {}", fixtures.len(), dir.display(), base);
    let report = run_all(&base, &fixtures);
    print!("{}", report.text);
    if report.failed == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// `contract/fixtures` relative to this crate, so the binary works from any working directory.
fn default_fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("fixtures")
}
