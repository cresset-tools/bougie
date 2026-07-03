//! Embeds git provenance into the `--version` string, uv-style:
//!
//! ```text
//! bougie 0.6.4 (63c5f57d3 2026-05-08 x86_64-unknown-linux-gnu)
//! ```
//!
//! The short SHA and commit date come from `git`; the target triple from
//! cargo's `TARGET`. When git metadata is unavailable (building from a release
//! tarball, a non-git checkout, or without `git` on `PATH`) the string degrades
//! to the bare `bougie 0.6.4`.

use std::process::Command;

/// Run `git <args>` and return its trimmed stdout, or `None` on any failure.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is always set");
    let target = std::env::var("TARGET").unwrap_or_default();

    // uv uses a 9-character short SHA (e.g. `63c5f57d3`).
    let sha = git(&["rev-parse", "--short=9", "HEAD"]);
    let date = git(&["log", "-1", "--date=short", "--format=%cd"]);

    // Also exposed on its own (`bougie_cli::BUILD_SHA`) for telemetry
    // and the daemon version report — `option_env!` resolves per
    // consuming crate, so the SHA has to flow through this crate's
    // build env rather than each consumer's.
    if let Some(sha) = &sha {
        println!("cargo:rustc-env=BOUGIE_BUILD_SHA={sha}");
    }

    let long = match (sha, date) {
        (Some(sha), Some(date)) => format!("{version} ({sha} {date} {target})"),
        _ => version,
    };
    println!("cargo:rustc-env=BOUGIE_LONG_VERSION={long}");

    // Rebuild when the checked-out commit moves so the embedded SHA stays
    // accurate. `--absolute-git-dir` resolves worktree `.git` files too.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
        if let Some(head_ref) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
            println!("cargo:rerun-if-changed={git_dir}/{head_ref}");
        }
    }
}
