//! Build and package one release artefact.
//!
//! The release workflow's matrix comes from `ci/release.toml`, and so does this command,
//! so the set of targets is stated once. Everything here is local-runnable: `cargo xtask
//! dist --target aarch64-unknown-linux-musl` produces exactly the artefact CI would.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Config {
    release: Release,
    #[serde(rename = "target")]
    targets: Vec<Target>,
}

#[derive(Debug, Deserialize)]
struct Release {
    binary: String,
    package: String,
    crate_manifest: String,
    profile: String,
}

#[derive(Debug, Deserialize)]
struct Target {
    triple: String,
    label: String,
    #[serde(default)]
    docker_platform: String,
    #[serde(default)]
    deb_arch: String,
    #[serde(default)]
    rpm_arch: String,
    #[serde(default)]
    arch_arch: String,
}

fn load_config() -> Result<Config, String> {
    let path = crate::repo_root().join("ci/release.toml");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))
}

/// Emits the release matrix as JSON for a GitHub Actions `strategy.matrix`, so the
/// workflow does not restate the target list and cannot drift from it.
///
/// `--linux-only` drops targets that produce no container image or package, which is how
/// the container job gets its platform list.
pub fn targets(args: &[String]) -> Result<(), String> {
    let cfg = load_config()?;
    let linux_only = args.iter().any(|a| a == "--linux-only");
    let entries: Vec<String> = cfg
        .targets
        .iter()
        .filter(|t| !linux_only || !t.docker_platform.is_empty())
        .map(|t| {
            format!(
                r#"{{"triple":"{}","label":"{}","docker_platform":"{}","deb_arch":"{}","rpm_arch":"{}","arch_arch":"{}"}}"#,
                t.triple, t.label, t.docker_platform, t.deb_arch, t.rpm_arch, t.arch_arch
            )
        })
        .collect();
    println!("{{\"include\":[{}]}}", entries.join(","));
    Ok(())
}

pub fn main(args: &[String]) -> Result<(), String> {
    let triple = crate::flag_value(args, "--target")
        .ok_or_else(|| "usage: cargo xtask dist --target <triple> [--version <v>]".to_string())?;
    let version = crate::flag_value(args, "--version").unwrap_or_else(|| "0.0.0-dev".to_string());
    let out_dir = crate::flag_value(args, "--out").unwrap_or_else(|| "dist".to_string());

    let root = crate::repo_root();
    let cfg = load_config()?;

    let target = cfg
        .targets
        .iter()
        .find(|t| t.triple == triple)
        .ok_or_else(|| {
            format!(
                "{triple} is not in ci/release.toml. Known targets: {}",
                cfg.targets
                    .iter()
                    .map(|t| t.triple.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    if !root.join(&cfg.release.crate_manifest).exists() {
        return Err(format!(
            "{} does not exist, so there is no daemon to release yet.\n  \
             The crate layout under crates/ is W3 §1's decision (DESIGN.md R-4). The \
             cross-compilation toolchains this pipeline needs are verified independently \
             of it — see ci/spike/armv6-cross-spike.sh and docs/release.md Appendix A.",
            cfg.release.crate_manifest
        ));
    }

    let builder = builder_for(&triple);
    println!(
        "building {} for {triple} with {}",
        cfg.release.package, builder
    );
    let status = Command::new(&builder)
        .current_dir(&root)
        .args([
            "build",
            "--locked",
            "--profile",
            &cfg.release.profile,
            "--package",
            &cfg.release.package,
            "--target",
            &triple,
        ])
        .status()
        .map_err(|e| format!("running {builder}: {e}"))?;
    if !status.success() {
        return Err(format!("{builder} build exited with {status}"));
    }

    let windows = triple.contains("windows");
    let bin_name = if windows {
        format!("{}.exe", cfg.release.binary)
    } else {
        cfg.release.binary.clone()
    };
    let built = root
        .join("target")
        .join(&triple)
        .join(&cfg.release.profile)
        .join(&bin_name);
    if !built.exists() {
        return Err(format!(
            "expected {} to exist after the build",
            built.display()
        ));
    }

    let out = root.join(&out_dir);
    std::fs::create_dir_all(&out).map_err(|e| format!("creating {}: {e}", out.display()))?;
    let stem = format!("{}_{}_{}", cfg.release.binary, version, target.label);
    let archive = if windows {
        pack_zip(&out, &stem, &built, &bin_name)?
    } else {
        pack_tar(&out, &stem, &built, &bin_name)?
    };
    let digest = sha256(&archive)?;
    std::fs::write(
        archive.with_extension(format!(
            "{}.sha256",
            archive.extension().unwrap_or_default().to_string_lossy()
        )),
        format!("{digest}  {}\n", file_name(&archive)),
    )
    .map_err(|e| format!("writing the checksum: {e}"))?;

    println!("{}  {digest}", archive.display());
    Ok(())
}

/// `cross` is used for every target that is not the host, because it carries the ARMv6 and
/// musl toolchains. Overriding it with CARGO is what lets a developer build the host target
/// without Docker.
fn builder_for(triple: &str) -> String {
    if let Ok(explicit) = std::env::var("MW_BUILDER") {
        return explicit;
    }
    let host = std::env::var("MW_HOST_TRIPLE").unwrap_or_default();
    if !host.is_empty() && host == triple {
        return std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    }
    "cross".to_string()
}

fn pack_tar(out: &Path, stem: &str, built: &Path, bin_name: &str) -> Result<PathBuf, String> {
    let staging = out.join(stem);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("staging: {e}"))?;
    std::fs::copy(built, staging.join(bin_name)).map_err(|e| format!("copying the binary: {e}"))?;
    for extra in ["README.md", "LICENSE", "docs/release.md"] {
        let src = crate::repo_root().join(extra);
        if src.exists() {
            let dst = staging.join(file_name(&src));
            std::fs::copy(&src, &dst).map_err(|e| format!("copying {extra}: {e}"))?;
        }
    }
    let archive = out.join(format!("{stem}.tar.gz"));
    // --sort, --mtime and the ownership flags make the archive byte-identical for the same
    // input, which is what lets a rebuilt tag be compared against a published one.
    let status = Command::new("tar")
        .current_dir(out)
        .args([
            "--sort=name",
            "--mtime=@0",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "-czf",
        ])
        .arg(&archive)
        .arg(stem)
        .status()
        .map_err(|e| format!("running tar: {e}"))?;
    if !status.success() {
        return Err(format!("tar exited with {status}"));
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok(archive)
}

fn pack_zip(out: &Path, stem: &str, built: &Path, bin_name: &str) -> Result<PathBuf, String> {
    let staging = out.join(stem);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("staging: {e}"))?;
    std::fs::copy(built, staging.join(bin_name)).map_err(|e| format!("copying the binary: {e}"))?;
    let archive = out.join(format!("{stem}.zip"));
    let status = Command::new("zip")
        .current_dir(out)
        .args(["-qr", "-X"])
        .arg(&archive)
        .arg(stem)
        .status()
        .map_err(|e| format!("running zip: {e}"))?;
    if !status.success() {
        return Err(format!("zip exited with {status}"));
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok(archive)
}

fn sha256(path: &Path) -> Result<String, String> {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|e| format!("running sha256sum: {e}"))?;
    if !out.status.success() {
        return Err(format!("sha256sum exited with {}", out.status));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| "sha256sum produced no digest".to_string())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}
