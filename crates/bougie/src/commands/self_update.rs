//! `bougie self update` — pull the latest published release for the
//! running target and atomically replace the on-disk binary (plus
//! `bgx` if it lives next to it).
//!
//! Sources, in order:
//!   1. `https://releases.bougie.tools/github/bougie/releases/download/<tag>/...`
//!      — the bougie.tools mirror, same URL the shell installer hits first.
//!   2. `https://github.com/cresset-tools/bougie/releases/download/<tag>/...`
//!      — canonical GitHub Release, fallback when the mirror is down or
//!      hasn't ingested the new tag yet.
//!
//! Archives are SHA-256-verified against the `.sha256` sidecar dist
//! publishes alongside each archive.
//!
//! Self-update only manages a binary that bougie's own installer placed.
//! The installer (cargo-dist `curl … | sh` / `irm … | iex`) drops a JSON
//! receipt next to its config — `<config>/bougie/bougie-receipt.json` —
//! recording the `install_prefix` it wrote the binary into. Before
//! touching anything we confirm the running binary lives at that prefix;
//! a copy from a package manager (apt/brew), `cargo install`, or nix
//! lives elsewhere and should be updated through that tool, so we refuse
//! (overridable with `--force`). This is the same receipt cargo-dist's
//! own updater keys off.
//!
//! The on-disk swap is atomic on Unix (`rename(2)` over the existing
//! `bougie` within the same directory). Windows can't unlink a running
//! executable, so we rename the current binary to `<name>.old.exe` in
//! the same directory first, then move the new one into place; any
//! pre-existing `*.old.exe` from a prior run is cleaned up at the
//! start.

use std::env;
use std::fs;
use std::fmt::Write as _;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use bougie_errors::BougieError;
use bougie_platform::target::Triple;
use eyre::{Result, WrapErr, eyre};
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const GITHUB_API_LATEST: &str =
    "https://api.github.com/repos/cresset-tools/bougie/releases/latest";
const MIRROR_BASE: &str = "https://releases.bougie.tools/github/bougie/releases/download";
const GITHUB_BASE: &str = "https://github.com/cresset-tools/bougie/releases/download";
const TAG_PREFIX: &str = "bougie-v";
const NO_SELF_UPDATE_ENV: &str = "BOUGIE_NO_SELF_UPDATE";

// Clippy wants `Duration::from_mins`, which is still unstable.
#[allow(clippy::duration_suboptimal_units)]
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

pub fn run(force: bool) -> Result<ExitCode> {
    if env::var_os(NO_SELF_UPDATE_ENV).is_some() {
        return Err(BougieError::SelfUpdate {
            detail: format!(
                "{NO_SELF_UPDATE_ENV} is set; refusing to self-update — \
                 the binary is owned by an external installer (package manager, \
                 nix store, /usr/local/bin, …)."
            ),
        }
        .into());
    }

    let current = env!("CARGO_PKG_VERSION");

    let current_exe = env::current_exe().wrap_err("locating the current bougie binary")?;

    // Only manage a binary bougie's own installer placed. A copy from a
    // package manager / cargo / nix lives outside the install receipt's
    // prefix; updating it in place would fight whatever owns it.
    match install_provenance(&current_exe) {
        Provenance::Managed => {}
        Provenance::External(reason) if force => {
            println!("warning: {reason}");
            println!("proceeding anyway because --force was given");
        }
        Provenance::External(reason) => {
            return Err(BougieError::SelfUpdate {
                detail: format!(
                    "{reason}. `bougie self update` only updates a binary installed by \
                     bougie's own installer (the cargo-dist `curl … | sh` / `irm … | iex` \
                     script). Update this copy through whatever installed it (apt, brew, \
                     nix, `cargo install`, …), or re-run with `--force` if you're sure it \
                     came from bougie's installer."
                ),
            }
            .into());
        }
    }

    let target = detect_target()?;
    let archive = archive_filename(&target);

    println!("bougie self update: current {current}, target {target}");

    let client = build_client(current)?;

    let latest = fetch_latest_version(&client)?;
    if !is_newer(&latest, current)? {
        println!("already up to date ({current})");
        return Ok(ExitCode::SUCCESS);
    }
    println!("latest:  {latest}");

    let bin_dir = current_exe
        .parent()
        .ok_or_else(|| eyre!("current bougie path has no parent dir: {}", current_exe.display()))?
        .to_path_buf();

    // On Windows, clear any leftover `*.old.exe` siblings from a prior update
    // before we create a new one for this run.
    #[cfg(windows)]
    purge_old(&bin_dir);

    let tmp = tempfile::TempDir::new().wrap_err("creating temp dir for self-update download")?;
    let archive_path = tmp.path().join(&archive);
    let sha_path = tmp.path().join(format!("{archive}.sha256"));
    let extract_root = tmp.path().join("extracted");
    fs::create_dir_all(&extract_root).wrap_err("preparing extract dir")?;

    let tag = format!("{TAG_PREFIX}{latest}");

    download(
        &client,
        &mirror_then_github(&tag, &format!("{archive}.sha256")),
        &sha_path,
    )
    .wrap_err("downloading sha256 sidecar")?;
    let expected_sha = parse_sidecar(&sha_path, &archive)?;

    println!("downloading {archive}");
    download(&client, &mirror_then_github(&tag, &archive), &archive_path)
        .wrap_err("downloading archive")?;
    verify_sha256(&archive_path, &expected_sha)?;

    extract(&archive_path, &extract_root)?;

    // dist packs archives as `bougie-<target>/{bougie,bgx}`.
    let stage = extract_root.join(format!("bougie-{target}"));
    let new_bougie = stage.join(binary_name("bougie"));
    if !new_bougie.exists() {
        return Err(eyre!(
            "extracted archive missing expected binary at {}",
            new_bougie.display()
        ));
    }
    replace(&new_bougie, &current_exe)?;

    // `bgx` ships in the same archive. Only replace if the user already has
    // one next to `bougie` — installs from `cargo install -p bougie` build
    // both, but package-manager installs that ship only `bougie` shouldn't
    // suddenly grow a stray `bgx`.
    let bgx_target = bin_dir.join(binary_name("bgx"));
    let new_bgx = stage.join(binary_name("bgx"));
    if bgx_target.exists() && new_bgx.exists() {
        replace(&new_bgx, &bgx_target)?;
    }

    println!("updated bougie {current} -> {latest}");
    Ok(ExitCode::SUCCESS)
}

/// Where the running binary came from, as far as the install receipt
/// can tell.
enum Provenance {
    /// Lives at the prefix bougie's installer recorded — ours to update.
    Managed,
    /// No matching receipt, or the binary runs from somewhere the
    /// receipt doesn't own. The string explains why, for the user.
    External(String),
}

/// Subset of cargo-dist's install receipt we care about. The installer
/// writes the full struct; we only read where it dropped the binary.
#[derive(Deserialize)]
struct InstallReceipt {
    install_prefix: String,
}

/// Decide whether `current_exe` is the binary bougie's installer
/// manages. The installer writes `<config>/bougie/bougie-receipt.json`
/// with the `install_prefix` it used; we treat the running binary as
/// managed when it sits at that prefix (cargo-dist's `flat` layout drops
/// binaries directly in the prefix; `unspecified`/cargo-home layouts use
/// `<prefix>/bin`, so accept both).
fn install_provenance(current_exe: &Path) -> Provenance {
    let Some(receipt_path) = receipt_path() else {
        return Provenance::External(
            "couldn't resolve a home/config directory to look for bougie's install receipt".into(),
        );
    };

    let Ok(body) = fs::read_to_string(&receipt_path) else {
        return Provenance::External(format!(
            "no bougie install receipt at {} — this binary doesn't look like it came from \
             bougie's installer",
            receipt_path.display()
        ));
    };

    let receipt: InstallReceipt = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return Provenance::External(format!(
                "bougie install receipt at {} is unreadable ({e})",
                receipt_path.display()
            ));
        }
    };

    if prefix_owns_exe(&receipt.install_prefix, current_exe) {
        Provenance::Managed
    } else {
        Provenance::External(format!(
            "this binary runs from {} but bougie's installer manages {} (per {}) — looks like it \
             was installed by a package manager, cargo, or nix",
            current_exe
                .parent()
                .unwrap_or(current_exe)
                .display(),
            receipt.install_prefix,
            receipt_path.display(),
        ))
    }
}

/// Does `install_prefix` from the receipt own `current_exe`? True when
/// the binary sits directly in the prefix (cargo-dist `flat` layout) or
/// in `<prefix>/bin` (cargo-home-style layouts). Both sides are
/// canonicalized so symlinked or `..`-laden paths still compare equal.
fn prefix_owns_exe(install_prefix: &str, current_exe: &Path) -> bool {
    let prefix = canonical_lossy(Path::new(install_prefix));
    let Some(exe_dir) = current_exe.parent().map(canonical_lossy) else {
        return false;
    };
    exe_dir == prefix || (exe_dir.ends_with("bin") && exe_dir.parent() == Some(prefix.as_path()))
}

/// Path to cargo-dist's install receipt for bougie. Mirrors the
/// installer scripts: the Unix `sh` installer writes under
/// `${XDG_CONFIG_HOME:-$HOME/.config}` (macOS included — it's a POSIX
/// script, not Apple `dirs` semantics); the PowerShell installer writes
/// under `%LOCALAPPDATA%`.
fn receipt_path() -> Option<PathBuf> {
    let config_dir = {
        #[cfg(unix)]
        {
            env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?
        }
        #[cfg(windows)]
        {
            PathBuf::from(env::var_os("LOCALAPPDATA")?)
        }
    };
    Some(config_dir.join("bougie").join("bougie-receipt.json"))
}

/// Canonicalize for comparison, falling back to the path as-given when
/// it can't be resolved (e.g. the recorded prefix no longer exists).
fn canonical_lossy(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
}

fn build_client(version: &str) -> Result<reqwest::blocking::Client> {
    let ua = format!("bougie/{version} (+https://github.com/cresset-tools/bougie)");
    reqwest::blocking::Client::builder()
        .user_agent(ua)
        .timeout(HTTP_TIMEOUT)
        .build()
        .wrap_err("building HTTP client")
}

fn fetch_latest_version(client: &reqwest::blocking::Client) -> Result<String> {
    let release: GhRelease = client
        .get(GITHUB_API_LATEST)
        .send()
        .wrap_err("querying GitHub Releases API")?
        .error_for_status()
        .wrap_err("GitHub Releases API returned an error status")?
        .json()
        .wrap_err("parsing GitHub Releases API response")?;
    release
        .tag_name
        .strip_prefix(TAG_PREFIX)
        .map(str::to_owned)
        .ok_or_else(|| eyre!("unexpected tag format from GitHub API: {}", release.tag_name))
}

fn detect_target() -> Result<String> {
    let triple = Triple::detect().wrap_err("detecting host target triple")?;
    Ok(triple.to_string())
}

fn archive_filename(target: &str) -> String {
    let ext = if cfg!(windows) { ".zip" } else { ".tar.gz" };
    format!("bougie-{target}{ext}")
}

fn binary_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}

fn mirror_then_github(tag: &str, file: &str) -> Vec<String> {
    vec![
        format!("{MIRROR_BASE}/{tag}/{file}"),
        format!("{GITHUB_BASE}/{tag}/{file}"),
    ]
}

fn download(client: &reqwest::blocking::Client, urls: &[String], dest: &Path) -> Result<()> {
    let mut last_err: Option<eyre::Report> = None;
    for url in urls {
        match try_download(client, url, dest) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(url = %url, error = %e, "self-update download attempt failed");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| eyre!("no download URLs provided")))
}

fn try_download(client: &reqwest::blocking::Client, url: &str, dest: &Path) -> Result<()> {
    let mut resp = client
        .get(url)
        .send()
        .wrap_err_with(|| format!("GET {url}"))?
        .error_for_status()
        .wrap_err_with(|| format!("HTTP error on {url}"))?;
    let mut out =
        fs::File::create(dest).wrap_err_with(|| format!("creating {}", dest.display()))?;
    io::copy(&mut resp, &mut out).wrap_err_with(|| format!("writing {}", dest.display()))?;
    Ok(())
}

/// Parse a `sha256sum` sidecar of the form `<hex> *<filename>` or
/// `<hex>  <filename>`. Accepts multi-line sidecars and matches by
/// filename when present.
fn parse_sidecar(path: &Path, archive_name: &str) -> Result<String> {
    let body = fs::read_to_string(path)
        .wrap_err_with(|| format!("reading sha256 sidecar at {}", path.display()))?;
    for line in body.lines() {
        let (hex, rest) = line.split_once(' ').unwrap_or((line, ""));
        let filename = rest.trim_start_matches([' ', '*']);
        if filename.is_empty() || filename == archive_name {
            return Ok(hex.to_lowercase());
        }
    }
    Err(eyre!(
        "no sha256 entry for {archive_name} in sidecar at {}",
        path.display()
    ))
}

fn verify_sha256(file: &Path, expected: &str) -> Result<()> {
    let mut hasher = Sha256::new();
    let mut f = fs::File::open(file).wrap_err_with(|| format!("opening {}", file.display()))?;
    // sha2 0.11 dropped the `io::Write` impl on `Sha256`; feed it from a
    // chunked read loop directly. 8 KiB stays under clippy's
    // `large_stack_arrays` threshold and matches `io::DEFAULT_BUF_SIZE`.
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
    let digest = hasher.finalize();
    let actual = digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    });
    let expected_lc = expected.to_lowercase();
    if actual != expected_lc {
        return Err(BougieError::SelfUpdate {
            detail: format!(
                "sha256 mismatch on {}: expected {expected_lc}, got {actual}",
                file.display()
            ),
        }
        .into());
    }
    Ok(())
}

fn extract(archive: &Path, into: &Path) -> Result<()> {
    let file =
        fs::File::open(archive).wrap_err_with(|| format!("opening {}", archive.display()))?;
    if archive.extension().and_then(|s| s.to_str()) == Some("zip") {
        let mut zip = zip::ZipArchive::new(file).wrap_err("opening zip archive")?;
        zip.extract(into)
            .wrap_err_with(|| format!("extracting zip into {}", into.display()))?;
    } else {
        let dec = GzDecoder::new(file);
        let mut ar = tar::Archive::new(dec);
        ar.unpack(into)
            .wrap_err_with(|| format!("extracting tar.gz into {}", into.display()))?;
    }
    Ok(())
}

fn replace(new: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        replace_unix(new, target)
    }
    #[cfg(windows)]
    {
        replace_windows(new, target)
    }
}

#[cfg(unix)]
fn replace_unix(new: &Path, target: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(target)
        .wrap_err_with(|| format!("stat {}", target.display()))?
        .permissions()
        .mode();

    // Stage the new binary as a hidden sibling so the rename is
    // guaranteed intra-filesystem and atomic. `fs::rename` across
    // filesystems falls back to copy+delete, which loses atomicity.
    let staging = target
        .parent()
        .ok_or_else(|| eyre!("target {} has no parent", target.display()))?
        .join(format!(
            ".{}.new",
            target.file_name().and_then(|s| s.to_str()).unwrap_or("bougie")
        ));

    fs::copy(new, &staging)
        .wrap_err_with(|| format!("staging new binary at {}", staging.display()))?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(mode))
        .wrap_err_with(|| format!("preserving mode on {}", staging.display()))?;

    fs::rename(&staging, target).wrap_err_with(|| {
        format!(
            "atomically renaming {} -> {}",
            staging.display(),
            target.display()
        )
    })?;
    Ok(())
}

#[cfg(windows)]
fn replace_windows(new: &Path, target: &Path) -> Result<()> {
    let stem = target
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("bougie");
    let old = target.with_file_name(format!("{stem}.old.exe"));
    let _ = fs::remove_file(&old);
    fs::rename(target, &old).wrap_err_with(|| {
        format!(
            "renaming current {} -> {} (Windows can't unlink a running exe)",
            target.display(),
            old.display()
        )
    })?;
    fs::copy(new, target)
        .wrap_err_with(|| format!("copying new binary to {}", target.display()))?;
    Ok(())
}

#[cfg(windows)]
fn purge_old(dir: &Path) {
    let Ok(read) = fs::read_dir(dir) else { return };
    for entry in read.flatten() {
        let p = entry.path();
        if p.file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name.ends_with(".old.exe"))
        {
            let _ = fs::remove_file(&p);
        }
    }
}

fn is_newer(latest: &str, current: &str) -> Result<bool> {
    Ok(parse_version(latest)? > parse_version(current)?)
}

/// Strict MAJOR.MINOR.PATCH parser. Pre-release/build metadata is
/// stripped from the patch component for ordering — `0.6.4-alpha.1` is
/// treated as `0.6.4`. Sufficient for dist's `bougie-v<x.y.z>` tags;
/// bougie doesn't publish pre-release dist channels.
fn parse_version(s: &str) -> Result<(u64, u64, u64)> {
    // Strip SemVer prerelease/build suffix before splitting on `.` — the
    // prerelease segment can itself contain dots (`0.6.4-alpha.1`) but is
    // separated from the core triple by `-` or `+`.
    let core = s.split(['-', '+']).next().unwrap_or(s);
    let mut parts = core.split('.');
    let major = parts.next().ok_or_else(|| eyre!("missing major in {s}"))?;
    let minor = parts.next().ok_or_else(|| eyre!("missing minor in {s}"))?;
    let patch = parts.next().ok_or_else(|| eyre!("missing patch in {s}"))?;
    if parts.next().is_some() {
        return Err(eyre!("too many version components in {s}"));
    }
    Ok((
        major.parse().wrap_err_with(|| format!("parsing major in {s}"))?,
        minor.parse().wrap_err_with(|| format!("parsing minor in {s}"))?,
        patch.parse().wrap_err_with(|| format!("parsing patch in {s}"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_filename_uses_platform_extension() {
        let f = archive_filename("x86_64-unknown-linux-gnu");
        let expected = if cfg!(windows) {
            "bougie-x86_64-unknown-linux-gnu.zip"
        } else {
            "bougie-x86_64-unknown-linux-gnu.tar.gz"
        };
        assert_eq!(f, expected);
    }

    #[test]
    fn binary_name_adds_exe_on_windows() {
        let name = binary_name("bougie");
        if cfg!(windows) {
            assert_eq!(name, "bougie.exe");
        } else {
            assert_eq!(name, "bougie");
        }
    }

    #[test]
    fn version_ordering_is_numeric_not_lexical() {
        assert!(is_newer("0.6.5", "0.6.4").unwrap());
        assert!(is_newer("0.10.0", "0.9.9").unwrap());
        assert!(!is_newer("0.6.4", "0.6.4").unwrap());
        assert!(!is_newer("0.6.3", "0.6.4").unwrap());
        assert!(is_newer("1.0.0", "0.99.99").unwrap());
    }

    #[test]
    fn version_strips_prerelease_suffix() {
        assert_eq!(parse_version("0.6.4-alpha.1").unwrap(), (0, 6, 4));
        assert_eq!(parse_version("0.6.4+build.5").unwrap(), (0, 6, 4));
    }

    #[test]
    fn version_rejects_malformed() {
        assert!(parse_version("1.2").is_err());
        assert!(parse_version("1.2.3.4").is_err());
        assert!(parse_version("v1.2.3").is_err());
    }

    #[test]
    fn sidecar_parsing_handles_sha256sum_binary_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foo.sha256");
        fs::write(
            &path,
            "07f7ec2c7ed474e7ba87a3bcb3663852b8de636945dbc499ab0a4494ad81da20 *bougie-x86_64-unknown-linux-gnu.tar.gz\n",
        )
        .unwrap();
        let hex = parse_sidecar(&path, "bougie-x86_64-unknown-linux-gnu.tar.gz").unwrap();
        assert_eq!(
            hex,
            "07f7ec2c7ed474e7ba87a3bcb3663852b8de636945dbc499ab0a4494ad81da20"
        );
    }

    #[test]
    fn sidecar_parsing_handles_text_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foo.sha256");
        fs::write(&path, "abc123  bougie-target.tar.gz\n").unwrap();
        let hex = parse_sidecar(&path, "bougie-target.tar.gz").unwrap();
        assert_eq!(hex, "abc123");
    }

    #[test]
    fn sidecar_parsing_picks_matching_filename_from_multi_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("foo.sha256");
        fs::write(
            &path,
            "deadbeef *something-else.tar.gz\ncafef00d *target.tar.gz\n",
        )
        .unwrap();
        let hex = parse_sidecar(&path, "target.tar.gz").unwrap();
        assert_eq!(hex, "cafef00d");
    }

    #[test]
    fn prefix_owns_exe_matches_flat_layout() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path();
        let exe = prefix.join(binary_name("bougie"));
        fs::write(&exe, b"").unwrap();
        assert!(prefix_owns_exe(&prefix.to_string_lossy(), &exe));
    }

    #[test]
    fn prefix_owns_exe_matches_bin_subdir_layout() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path();
        let bin = prefix.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(binary_name("bougie"));
        fs::write(&exe, b"").unwrap();
        assert!(prefix_owns_exe(&prefix.to_string_lossy(), &exe));
    }

    #[test]
    fn prefix_owns_exe_rejects_foreign_dir() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("managed");
        let elsewhere = dir.path().join("usr-bin");
        fs::create_dir_all(&prefix).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        let exe = elsewhere.join(binary_name("bougie"));
        fs::write(&exe, b"").unwrap();
        // A package-manager copy in a sibling dir is not owned by the
        // installer's prefix.
        assert!(!prefix_owns_exe(&prefix.to_string_lossy(), &exe));
    }

    #[test]
    fn mirror_is_tried_before_github() {
        let urls = mirror_then_github("bougie-v0.6.4", "bougie-x86_64-unknown-linux-gnu.tar.gz");
        assert_eq!(urls.len(), 2);
        assert!(urls[0].starts_with("https://releases.bougie.tools/"));
        assert!(urls[1].starts_with("https://github.com/"));
        assert!(urls[0].ends_with("/bougie-v0.6.4/bougie-x86_64-unknown-linux-gnu.tar.gz"));
        assert!(urls[1].ends_with("/bougie-v0.6.4/bougie-x86_64-unknown-linux-gnu.tar.gz"));
    }
}
