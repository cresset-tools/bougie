//! `bougie format` — format the project's PHP the way `uv format` runs
//! ruff: bougie does not bundle a formatter. It downloads a *pinned*
//! `wick` binary on first use (cresset-tools/wick — an unconfigurable,
//! Laravel Pint-style formatter), caches it, and execs it. Every
//! argument is forwarded verbatim to `wick`, so `bougie format`,
//! `bougie format --check`, `bougie format src/ --diff`, and
//! `… | bougie format -` all behave exactly like the matching `wick`
//! invocation.
//!
//! Pinning the version (overridable with `BOUGIE_WICK_VERSION`) keeps
//! formatting stable for a given bougie release — the same property
//! `uv format` gets by pinning a default ruff. The binary is fetched
//! from the same mirror→GitHub pair `bougie self update` uses, and
//! SHA-256-verified against the `.sha256` sidecar dist publishes.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use bougie_platform::target::Triple;
use eyre::{Result, WrapErr, eyre};
use std::ffi::OsString;

use super::native_fetch;
use bougie_paths::Paths;

/// Default pinned wick version (overridable with `BOUGIE_WICK_VERSION`).
/// Bump in lockstep with the wick release bougie should ship against; the
/// pinned version must have published prebuilt binaries.
const DEFAULT_WICK_VERSION: &str = "0.2.3";
const WICK_VERSION_ENV: &str = "BOUGIE_WICK_VERSION";

const MIRROR_BASE: &str = "https://releases.bougie.tools/github/wick/releases/download";
const GITHUB_BASE: &str = "https://github.com/cresset-tools/wick/releases/download";
const TAG_PREFIX: &str = "wick-v";

pub fn run(args: &[OsString]) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let version = std::env::var(WICK_VERSION_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_WICK_VERSION.to_string());

    let wick = ensure_wick(&paths, &version)?;

    // Hand off to wick. Inherit stdio so `--diff` output, `--check`
    // results, and stdin piping all pass straight through, then mirror
    // wick's exit code (0 = formatted/clean, 1 = changes needed / parse
    // error) so `bougie format --check` works in CI just like `wick`.
    let status = Command::new(&wick)
        .args(args)
        .status()
        .wrap_err_with(|| format!("failed to execute wick at {}", wick.display()))?;

    // Mirror wick's exit code (0 = formatted/clean, 1 = changes needed /
    // parse error) so `bougie format --check` works in CI just like
    // `wick`. A signal-killed child (`code()` == None on Unix) maps to a
    // generic failure rather than pretending success.
    let code = status
        .code()
        .and_then(|c| u8::try_from(c).ok())
        .unwrap_or(1);
    Ok(ExitCode::from(code))
}

/// Return the path to a cached, ready-to-run `wick <version>` binary,
/// downloading and extracting it on a cache miss.
fn ensure_wick(paths: &Paths, version: &str) -> Result<PathBuf> {
    let bin = paths
        .cache()
        .join("wick")
        .join(version)
        .join(binary_name("wick"));
    if bin.is_file() {
        return Ok(bin);
    }

    let target = Triple::detect()?.to_string();
    let archive = archive_filename(&target);
    let tag = format!("{TAG_PREFIX}{version}");

    let client = bougie_fetch::default_client()?;

    let tmp = tempfile::TempDir::new().wrap_err("creating temp dir for wick download")?;
    let archive_path = tmp.path().join(&archive);
    let sha_path = tmp.path().join(format!("{archive}.sha256"));
    let extract_root = tmp.path().join("extracted");
    fs::create_dir_all(&extract_root).wrap_err("preparing extract dir")?;

    native_fetch::download(
        &client,
        &urls(&tag, &format!("{archive}.sha256")),
        &sha_path,
    )
    .wrap_err("downloading wick sha256 sidecar")?;
    let expected = native_fetch::parse_sidecar(&sha_path, &archive)?;

    println!("bougie format: fetching wick {version} ({target})");
    native_fetch::download(&client, &urls(&tag, &archive), &archive_path).wrap_err_with(|| {
        format!(
            "downloading wick {version}. If this 404s, that version may not have published \
             binaries yet — check https://github.com/cresset-tools/wick/releases or pin a \
             different version with {WICK_VERSION_ENV}."
        )
    })?;
    native_fetch::verify_sha256(&archive_path, &expected)?;

    native_fetch::extract(&archive_path, &extract_root, cfg!(windows))?;

    // dist packs archives as `wick-<target>/wick[.exe]`.
    let staged = extract_root
        .join(format!("wick-{target}"))
        .join(binary_name("wick"));
    if !staged.is_file() {
        return Err(eyre!(
            "extracted wick archive missing expected binary at {}",
            staged.display()
        ));
    }

    native_fetch::install_file_atomic(&staged, &bin)?;

    Ok(bin)
}

fn binary_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn archive_filename(target: &str) -> String {
    if cfg!(windows) {
        format!("wick-{target}.zip")
    } else {
        format!("wick-{target}.tar.gz")
    }
}

/// Mirror first (low latency, no GitHub anonymous rate limits), GitHub
/// release as fallback — same precedence as `bougie self update`.
fn urls(tag: &str, file: &str) -> Vec<String> {
    vec![
        format!("{MIRROR_BASE}/{tag}/{file}"),
        format!("{GITHUB_BASE}/{tag}/{file}"),
    ]
}
