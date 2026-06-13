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

use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use bougie_platform::target::Triple;
use eyre::{Result, WrapErr, eyre};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::ffi::OsString;

use bougie_paths::Paths;

/// Default pinned wick version. The first wick release carrying prebuilt
/// binaries is `wick-v0.2.0` (0.1.0 was a crates.io-only publish). Bump
/// in lockstep with the wick version bougie should ship against.
const DEFAULT_WICK_VERSION: &str = "0.2.0";
const WICK_VERSION_ENV: &str = "BOUGIE_WICK_VERSION";

const MIRROR_BASE: &str = "https://releases.bougie.tools/github/wick/releases/download";
const GITHUB_BASE: &str = "https://github.com/cresset-tools/wick/releases/download";
const TAG_PREFIX: &str = "wick-v";

#[allow(clippy::duration_suboptimal_units)]
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

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

    download(
        &client,
        &urls(&tag, &format!("{archive}.sha256")),
        &sha_path,
    )
    .wrap_err("downloading wick sha256 sidecar")?;
    let expected = parse_sidecar(&sha_path, &archive)?;

    println!("bougie format: fetching wick {version} ({target})");
    download(&client, &urls(&tag, &archive), &archive_path).wrap_err_with(|| {
        format!(
            "downloading wick {version}. If this 404s, that version may not have published \
             binaries yet — check https://github.com/cresset-tools/wick/releases or pin a \
             different version with {WICK_VERSION_ENV}."
        )
    })?;
    verify_sha256(&archive_path, &expected)?;

    extract(&archive_path, &extract_root)?;

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

    // Move into the cache atomically: stage in a sibling temp file in the
    // final directory, then rename over the target so a concurrent run
    // never sees a half-written binary.
    if let Some(parent) = bin.parent() {
        fs::create_dir_all(parent).wrap_err("creating wick cache dir")?;
    }
    let staging = bin.with_extension("partial");
    fs::copy(&staged, &staging).wrap_err("staging wick binary into cache")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))
            .wrap_err("marking wick executable")?;
    }
    fs::rename(&staging, &bin).wrap_err("installing wick into cache")?;

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

/// Try each URL in order; succeed on the first that downloads.
fn download(client: &reqwest::blocking::Client, urls: &[String], dest: &Path) -> Result<()> {
    let mut last_err = None;
    for url in urls {
        match try_download(client, url, dest) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| eyre!("no download URLs provided")))
}

fn try_download(client: &reqwest::blocking::Client, url: &str, dest: &Path) -> Result<()> {
    let resp = client
        .get(url)
        .timeout(HTTP_TIMEOUT)
        .send()
        .wrap_err_with(|| format!("requesting {url}"))?
        .error_for_status()
        .wrap_err_with(|| format!("fetching {url}"))?;
    let bytes = resp
        .bytes()
        .wrap_err_with(|| format!("reading body of {url}"))?;
    fs::write(dest, &bytes).wrap_err_with(|| format!("writing {}", dest.display()))?;
    Ok(())
}

/// A `.sha256` sidecar is `<hex>  <filename>`; take the leading hex token.
fn parse_sidecar(path: &Path, archive_name: &str) -> Result<String> {
    let body = fs::read_to_string(path).wrap_err("reading sha256 sidecar")?;
    let hex = body
        .split_whitespace()
        .next()
        .ok_or_else(|| eyre!("empty sha256 sidecar for {archive_name}"))?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(eyre!(
            "malformed sha256 in sidecar for {archive_name}: {hex:?}"
        ));
    }
    Ok(hex.to_ascii_lowercase())
}

fn verify_sha256(file: &Path, expected: &str) -> Result<()> {
    let mut f = fs::File::open(file).wrap_err_with(|| format!("opening {}", file.display()))?;
    let mut hasher = Sha256::new();
    // sha2 0.11 dropped the `io::Write` impl, so feed it from a chunked
    // read loop (same approach as `self_update::verify_sha256`).
    let mut buf = [0u8; 8 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .wrap_err_with(|| format!("reading {}", file.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        });
    if actual != expected {
        return Err(eyre!(
            "sha256 mismatch for {}: expected {expected}, got {actual}",
            file.display()
        ));
    }
    Ok(())
}

fn extract(archive: &Path, into: &Path) -> Result<()> {
    let file =
        fs::File::open(archive).wrap_err_with(|| format!("opening {}", archive.display()))?;
    if cfg!(windows) {
        let mut zip = zip::ZipArchive::new(file).wrap_err("opening wick zip archive")?;
        zip.extract(into).wrap_err("extracting wick zip archive")?;
    } else {
        let dec = GzDecoder::new(file);
        let mut ar = tar::Archive::new(dec);
        ar.unpack(into).wrap_err("extracting wick tar.gz archive")?;
    }
    Ok(())
}
