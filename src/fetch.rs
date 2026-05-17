//! Atomic blob fetch + extract per CLI.md §7.4.
//!
//! Pattern: stream into `$BOUGIE_CACHE/blobs/<sha256>.partial`, verify
//! sha256 while writing, extract into `<dest>.incoming` (sibling of
//! the final destination so the rename is on the same filesystem),
//! atomic-rename to `<dest>`, delete `tmp`.

use crate::errors::BougieError;
use eyre::{Result, WrapErr};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{copy, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Aggregate download progress bar shared across many `fetch_blob`/
/// `fetch_file` calls. Renders a single bar with the running label of
/// the part currently in flight; orchestrators (e.g. baseline install,
/// composer-required extensions) drive *one* `DownloadBar` across the
/// whole loop so the user sees a single combined bar instead of one
/// per artifact.
///
/// The bar starts with length 0; callers grow the planned total via
/// [`Self::add_planned`] as each manifest reveals more bytes (the
/// index ships `size` on every blob, so no HEAD round-trips are
/// needed). [`Self::set_current`] sets the right-hand-side label
/// shown for the artifact currently downloading.
///
/// Hidden bars (non-TTY stderr, `--quiet`, `--format json-v1`) accept
/// every method as a no-op so the byte-copy loop in `fetch.rs` stays
/// branch-free.
#[derive(Debug)]
pub struct DownloadBar {
    pb: ProgressBar,
}

const RETRY_BUDGET: u32 = 1;

/// Archive format `fetch_blob` should decode. Selected per-call
/// because the index advertises tar.zst for bougie-published artifacts
/// while windows.php.net (Phase 3+) publishes zip. `fetch_file`
/// doesn't extract and ignores this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    /// Zstandard-compressed POSIX tar — bougie's own publish format.
    TarZst,
    /// Zip — windows.php.net's interpreter + PECL distribution format.
    Zip,
}

#[derive(Debug, Clone)]
pub struct BlobSpec<'a> {
    pub url: &'a str,
    pub sha256: &'a str,
    pub partial_dir: &'a Path,
    pub dest: &'a Path,
    /// Leading path component to strip from every entry while
    /// extracting. Interpreter tarballs wrap their contents in
    /// `install/`; per-store-path closure tarballs wrap theirs in
    /// `<storeName>/` (see `shared/tarball-store-path.nix`).
    /// windows.php.net's `php-<ver>-Win32-...zip` wraps contents in
    /// `php-<ver>/`. Pass `""` for unwrapped archives (e.g.
    /// per-extension blobs that ship `lib/extensions/<api>/<name>.so`
    /// at the top level).
    pub strip_prefix: &'a str,
    /// How to decode the downloaded bytes. Ignored by [`fetch_file`].
    pub archive: ArchiveKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobOutcome {
    AlreadyPresent,
    Downloaded,
}

/// Fetch + extract one tar.zst blob. No-op if `dest` exists.
///
/// `bar` is the caller-owned aggregate bar that this call advances as
/// bytes arrive. Set the part label via [`DownloadBar::set_current`]
/// *before* calling so the right-hand `{msg}` shows the artifact name
/// for the duration of the transfer. Pass [`DownloadBar::hidden`] when
/// the caller has no UI of its own — the byte-copy loop stays the
/// same shape either way.
pub fn fetch_blob(
    client: &reqwest::blocking::Client,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
) -> Result<BlobOutcome> {
    fetch_with_retry(client, spec, bar, try_once_blob)
}

/// Fetch a single bare file (e.g. a `.phar`) into `dest`, verifying its
/// sha256. No tar/zst extraction; the verified bytes are placed at `dest`
/// atomically. No-op if `dest` exists.
pub fn fetch_file(
    client: &reqwest::blocking::Client,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
) -> Result<BlobOutcome> {
    fetch_with_retry(client, spec, bar, try_once_file)
}

fn fetch_with_retry(
    client: &reqwest::blocking::Client,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
    once: fn(&reqwest::blocking::Client, &BlobSpec<'_>, &DownloadBar) -> Result<()>,
) -> Result<BlobOutcome> {
    if spec.dest.exists() {
        return Ok(BlobOutcome::AlreadyPresent);
    }
    fs::create_dir_all(spec.partial_dir)
        .wrap_err_with(|| format!("creating {}", spec.partial_dir.display()))?;
    if let Some(parent) = spec.dest.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }

    let mut attempts = 0;
    loop {
        match once(client, spec, bar) {
            Ok(()) => return Ok(BlobOutcome::Downloaded),
            Err(e) if attempts < RETRY_BUDGET => {
                attempts += 1;
                tracing::warn!(error = %e, attempt = attempts, "blob fetch failed; retrying");
            }
            Err(e) => return Err(e),
        }
    }
}

/// Stream the blob into `<partial_dir>/<sha>.partial`, hashing as we go.
/// Returns the path to the verified partial on success; deletes it and
/// errors on hash mismatch.
fn fetch_to_partial(
    client: &reqwest::blocking::Client,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
) -> Result<PathBuf> {
    let tmp = spec.partial_dir.join(format!("{}.partial", spec.sha256));

    let mut resp = client.get(spec.url).send().map_err(|e| BougieError::Network {
        operation: format!("fetching blob {}", spec.url),
        detail: e.to_string(),
    })?;
    if !resp.status().is_success() {
        return Err(BougieError::Network {
            operation: format!("GET {}", spec.url),
            detail: format!("server returned HTTP {}", resp.status()),
        }
        .into());
    }

    let mut file = File::create(&tmp).wrap_err_with(|| format!("creating {}", tmp.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = resp.read(&mut buf).map_err(|e| BougieError::Network {
            operation: format!("reading blob body from {}", spec.url),
            detail: e.to_string(),
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .wrap_err_with(|| format!("writing {}", tmp.display()))?;
        bar.inc(n as u64);
    }
    file.flush().wrap_err("flushing partial blob")?;

    let actual = format_hex(&hasher.finalize());
    if !actual.eq_ignore_ascii_case(spec.sha256) {
        let _ = fs::remove_file(&tmp);
        return Err(BougieError::BlobHashMismatch {
            url: spec.url.to_owned(),
            expected: spec.sha256.to_owned(),
            actual,
        }
        .into());
    }
    Ok(tmp)
}

fn try_once_blob(
    client: &reqwest::blocking::Client,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
) -> Result<()> {
    let tmp = fetch_to_partial(client, spec, bar)?;

    let incoming = sibling_with_suffix(spec.dest, ".incoming");
    let _ = fs::remove_dir_all(&incoming);
    fs::create_dir_all(&incoming)
        .wrap_err_with(|| format!("creating {}", incoming.display()))?;
    match spec.archive {
        ArchiveKind::TarZst => extract_tar_zst(&tmp, &incoming, spec.strip_prefix)?,
        ArchiveKind::Zip => extract_zip(&tmp, &incoming, spec.strip_prefix)?,
    }

    fs::rename(&incoming, spec.dest)
        .wrap_err_with(|| format!("rename {} → {}", incoming.display(), spec.dest.display()))?;
    let _ = fs::remove_file(&tmp);
    Ok(())
}

fn try_once_file(
    client: &reqwest::blocking::Client,
    spec: &BlobSpec<'_>,
    bar: &DownloadBar,
) -> Result<()> {
    let tmp = fetch_to_partial(client, spec, bar)?;

    // Stage the verified bytes as a sibling of `dest` so the rename is
    // always intra-filesystem, even when `partial_dir` is on a different
    // filesystem from `dest` (cache vs data, per CLI.md §7.4).
    let incoming = sibling_with_suffix(spec.dest, ".incoming");
    let _ = fs::remove_file(&incoming);
    fs::copy(&tmp, &incoming)
        .wrap_err_with(|| format!("staging {} → {}", tmp.display(), incoming.display()))?;
    fs::rename(&incoming, spec.dest)
        .wrap_err_with(|| format!("rename {} → {}", incoming.display(), spec.dest.display()))?;
    let _ = fs::remove_file(&tmp);
    Ok(())
}

/// Extract a `.tar.zst` archive into `into`, stripping `strip_prefix`
/// as a leading path component from every entry so e.g. the binary
/// that the archive ships at `install/bin/php` (with
/// `strip_prefix = "install"`) lands at `<into>/bin/php`. Entries
/// that don't start with `strip_prefix` pass through unchanged.
/// Pass `""` to disable stripping (archive entries land verbatim).
fn extract_tar_zst(tar_zst: &Path, into: &Path, strip_prefix: &str) -> Result<()> {
    let f = File::open(tar_zst)
        .wrap_err_with(|| format!("opening {}", tar_zst.display()))?;
    let zd = zstd::stream::read::Decoder::new(f).wrap_err("zstd decoder")?;
    let mut archive = tar::Archive::new(zd);
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    for entry in archive
        .entries()
        .wrap_err_with(|| format!("reading entries from {}", tar_zst.display()))?
    {
        let mut entry = entry.wrap_err("reading archive entry")?;
        let path = entry
            .path()
            .wrap_err("reading entry path")?
            .into_owned();
        let Some(rewritten) = rewrite_archive_path(&path, strip_prefix) else {
            // The prefix directory entry itself; skip — `into` exists.
            continue;
        };
        let dest = into.join(&rewritten);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        // Hardlink entries need their target rewritten by `strip_prefix`
        // too: the tar header records the link source as e.g.
        // `install/escript/rabbitmq-diagnostics`, but on disk the file
        // actually lives at `<into>/escript/rabbitmq-diagnostics` once
        // the prefix has been stripped. Letting `entry.unpack(&dest)`
        // handle the link would try to resolve the archive-internal
        // path relative to CWD and fail with ENOENT.
        if entry.header().entry_type().is_hard_link() {
            let link_name = entry
                .link_name()
                .wrap_err("reading hardlink target")?
                .ok_or_else(|| eyre::eyre!("hardlink entry for {} has no link name", path.display()))?
                .into_owned();
            let link_dest_rel = rewrite_archive_path(&link_name, strip_prefix)
                .ok_or_else(|| eyre::eyre!("hardlink target {} resolves to the strip prefix root", link_name.display()))?;
            let link_dest = into.join(&link_dest_rel);
            // Idempotency under `--overwrite`-style retries: a
            // previous half-finished extract may have left the link
            // already in place. Removing first matches what tar's
            // own unpack does (it sets overwrite=true by default in
            // 0.4).
            let _ = fs::remove_file(&dest);
            fs::hard_link(&link_dest, &dest).wrap_err_with(|| {
                format!(
                    "linking {} → {} (for archive entry {})",
                    dest.display(),
                    link_dest.display(),
                    path.display(),
                )
            })?;
            continue;
        }
        entry
            .unpack(&dest)
            .wrap_err_with(|| format!("unpacking {} → {}", path.display(), dest.display()))?;
    }
    Ok(())
}

/// Extract a `.zip` archive into `into`, stripping `strip_prefix` as a
/// leading path component from every entry (same convention as
/// [`extract_tar_zst`]). Used for windows.php.net interpreter ZIPs,
/// which wrap their contents in `php-<version>/`, and for PECL DLL
/// ZIPs, which are flat (`strip_prefix = ""`).
///
/// Symlink entries (only seen in unix-built ZIPs) are not expected on
/// the windows.php.net surface and are unpacked as plain files;
/// re-introduce a symlink branch when a non-windows ZIP source needs
/// it. The `zip` crate's own `ZipArchive::extract` would handle them
/// on Unix, but rolling the walk by hand here keeps the strip-prefix
/// rewrite (which `extract` doesn't support) trivial.
fn extract_zip(zip_path: &Path, into: &Path, strip_prefix: &str) -> Result<()> {
    let f = File::open(zip_path)
        .wrap_err_with(|| format!("opening {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(f)
        .wrap_err_with(|| format!("reading zip {}", zip_path.display()))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .wrap_err_with(|| format!("reading zip entry {i}"))?;
        // `enclosed_name` is the traversal-safe path (rejects `..` and
        // absolute paths) — `name()` would return the raw header bytes.
        let Some(raw) = entry.enclosed_name() else {
            continue;
        };
        let Some(rewritten) = rewrite_archive_path(&raw, strip_prefix) else {
            // The prefix directory entry itself; skip — `into` exists.
            continue;
        };
        let dest = into.join(&rewritten);
        if entry.is_dir() {
            fs::create_dir_all(&dest)
                .wrap_err_with(|| format!("creating {}", dest.display()))?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("creating {}", parent.display()))?;
        }
        let mut out = File::create(&dest)
            .wrap_err_with(|| format!("creating {}", dest.display()))?;
        std::io::copy(&mut entry, &mut out)
            .wrap_err_with(|| format!("writing {}", dest.display()))?;
        // Preserve Unix executable bit if the entry carried mode info
        // (windows.php.net ZIPs don't, but ZIPs built on Unix do).
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

/// Apply `strip_prefix` to a tar-internal path. Returns `None` when the
/// rewrite produces an empty path (the prefix directory entry itself
/// — caller skips it because the destination already exists). Entries
/// that don't start with the prefix are left alone.
fn rewrite_archive_path(path: &Path, strip_prefix: &str) -> Option<PathBuf> {
    let rewritten = if strip_prefix.is_empty() {
        path.to_path_buf()
    } else {
        match path.strip_prefix(strip_prefix) {
            Ok(rest) => rest.to_path_buf(),
            Err(_) => path.to_path_buf(),
        }
    };
    if rewritten.as_os_str().is_empty() {
        None
    } else {
        Some(rewritten)
    }
}

/// Stream `from` into `into` and verify its sha256. Used by callers
/// that already have the bytes locally (e.g. manifests).
pub fn copy_with_sha256<R: Read, W: Write>(from: &mut R, into: &mut W) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = from.read(&mut buf).wrap_err("reading source")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        into.write_all(&buf[..n]).wrap_err("writing dest")?;
    }
    Ok(format_hex(&hasher.finalize()))
}

fn format_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn sibling_with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let parent = p.parent().unwrap_or_else(|| Path::new(""));
    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("blob");
    parent.join(format!("{name}{suffix}"))
}

impl DownloadBar {
    /// Build an aggregate download bar that renders on stderr (when
    /// the global progress-visible flag is set) or stays hidden
    /// otherwise. Length starts at 0; call [`Self::add_planned`] as
    /// each manifest is parsed and the next chunk of expected bytes
    /// becomes known. Set the per-artifact label via
    /// [`Self::set_current`] before each `fetch_blob`/`fetch_file`.
    pub fn new(label: &str) -> Self {
        if !crate::output::progress_visible() {
            return Self::hidden();
        }
        let pb = ProgressBar::new(0);
        pb.set_draw_target(ProgressDrawTarget::stderr_with_hz(15));
        // Template-with-fallback: `indicatif`'s template parser is
        // pinned at build time, so a malformed template is a bug,
        // not a user-visible failure mode. `unwrap_or_else` keeps
        // us off the panic path even if a future edit breaks it.
        //
        // `progress_chars("--")` paints the entire bar with `-`; the
        // foreground/background colors in the `{bar}` token split it
        // into a magenta filled portion and a dim-grey unfilled tail.
        let style = ProgressStyle::with_template(
            "  {prefix:<12} {bar:32.magenta/white.dim} {bytes}/{total_bytes} ({bytes_per_sec}, {eta}) {msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("--");
        pb.set_style(style);
        pb.set_prefix(label.to_owned());
        pb.enable_steady_tick(Duration::from_millis(120));
        Self { pb }
    }

    /// A no-op bar. Use when a caller has no aggregate of its own but
    /// still needs to satisfy [`fetch_blob`] / [`fetch_file`].
    pub fn hidden() -> Self {
        let pb = ProgressBar::new(0);
        pb.set_draw_target(ProgressDrawTarget::hidden());
        Self { pb }
    }

    /// Grow the planned total. Safe to call repeatedly as each manifest
    /// reveals the next batch of bytes; calling with `0` is a no-op
    /// (older publishers may emit `size: 0` for backwards-compat —
    /// such contributions just don't extend the bar).
    pub fn add_planned(&self, bytes: u64) {
        if bytes > 0 {
            self.pb.inc_length(bytes);
        }
    }

    /// Current planned total. Returns 0 for a fresh / hidden bar that
    /// has never been planned against. Used in tests to assert
    /// planning correctness; not part of the user-facing UX.
    #[cfg(test)]
    pub(crate) fn planned(&self) -> u64 {
        self.pb.length().unwrap_or(0)
    }

    /// Set the right-hand-side label showing which artifact is
    /// currently downloading. Overwrites any previous label.
    pub fn set_current(&self, name: impl Into<String>) {
        self.pb.set_message(name.into());
    }

    /// Final flush — clears the bar from the terminal.
    pub fn finish(&self) {
        self.pb.finish_and_clear();
    }

    /// Advance the bar by `n` freshly-downloaded bytes. Called from
    /// the byte-copy loop in `fetch_to_partial`; not part of the
    /// public surface.
    fn inc(&self, n: u64) {
        self.pb.inc(n);
    }
}

/// Discard a partial download — used on cancellation / error
/// recovery (callers that know the blob is invalid).
pub fn discard_partial(partial_dir: &Path, sha256: &str) {
    let p = partial_dir.join(format!("{sha256}.partial"));
    let _ = fs::remove_file(p);
}

/// Like [`copy`] but consumes a known body and writes it; returns the
/// hex sha256 of the bytes written.
pub fn write_with_sha256(into: &Path, bytes: &[u8]) -> Result<String> {
    let mut f = File::create(into).wrap_err_with(|| format!("creating {}", into.display()))?;
    f.write_all(bytes).wrap_err("writing")?;
    let _ = copy(&mut std::io::empty(), &mut f); // ensure no warnings
    Ok(format_hex(&Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_hex_lowercase() {
        assert_eq!(format_hex(&[0xab, 0xcd]), "abcd");
        assert_eq!(format_hex(&[0]), "00");
    }

    #[test]
    fn download_bar_hidden_accepts_all_methods() {
        // Smoke test: a hidden bar must accept the full driver API
        // without panicking, so the byte-copy loop in non-TTY contexts
        // stays branch-free.
        let bar = DownloadBar::hidden();
        bar.add_planned(0);
        bar.add_planned(1024);
        bar.set_current("php-8.3.12");
        bar.inc(512);
        bar.finish();
    }

    #[test]
    fn sibling_with_suffix_appends() {
        let p = Path::new("/a/b/c");
        assert_eq!(sibling_with_suffix(p, ".incoming"), Path::new("/a/b/c.incoming"));
    }

    #[test]
    fn write_with_sha256_returns_correct_hash() {
        let dir = tempfile::TempDir::new().unwrap();
        let dest = dir.path().join("f");
        let h = write_with_sha256(&dest, b"hello").unwrap();
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn try_once_file_writes_verified_bytes_atomically() {
        let dir = tempfile::TempDir::new().unwrap();
        let partial_dir = dir.path().join("partial");
        std::fs::create_dir_all(&partial_dir).unwrap();
        let dest = dir.path().join("out").join("composer.phar");

        // Pre-stage a "downloaded" partial so try_once_file can act
        // on it without a real HTTP server.
        let body = b"#!/usr/bin/env php\n<?php echo 'hi';\n";
        let sha = format_hex(&Sha256::digest(body));
        let tmp = partial_dir.join(format!("{sha}.partial"));
        std::fs::write(&tmp, body).unwrap();

        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let incoming = sibling_with_suffix(&dest, ".incoming");
        let _ = std::fs::remove_file(&incoming);
        std::fs::copy(&tmp, &incoming).unwrap();
        std::fs::rename(&incoming, &dest).unwrap();
        let _ = std::fs::remove_file(&tmp);

        assert!(dest.is_file());
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!tmp.exists());
        assert!(!incoming.exists());
    }

    #[test]
    fn extract_strips_install_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let archive_path = dir.path().join("a.tar.zst");

        let mut tar_buf: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let mut header = tar::Header::new_gnu();
            header.set_size(5);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "install/bin/php", &b"hello"[..])
                .unwrap();
            let mut header2 = tar::Header::new_gnu();
            header2.set_size(2);
            header2.set_mode(0o644);
            header2.set_cksum();
            builder
                .append_data(&mut header2, "install/etc/php.ini", &b"hi"[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let zst = zstd::encode_all(&tar_buf[..], 0).unwrap();
        std::fs::write(&archive_path, zst).unwrap();

        let into = dir.path().join("out");
        std::fs::create_dir_all(&into).unwrap();
        extract_tar_zst(&archive_path, &into, "install").unwrap();

        assert!(into.join("bin/php").is_file());
        assert!(into.join("etc/php.ini").is_file());
        assert!(!into.join("install").exists());
    }

    #[test]
    fn extract_passes_through_when_no_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let archive_path = dir.path().join("a.tar.zst");

        let mut tar_buf: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "bin/php", &b"abc"[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let zst = zstd::encode_all(&tar_buf[..], 0).unwrap();
        std::fs::write(&archive_path, zst).unwrap();

        let into = dir.path().join("out");
        std::fs::create_dir_all(&into).unwrap();
        extract_tar_zst(&archive_path, &into, "install").unwrap();

        assert!(into.join("bin/php").is_file());
    }

    #[test]
    fn extract_strips_arbitrary_prefix() {
        // Closure tarballs wrap contents in `<storeName>/`; the
        // extractor must strip whatever prefix the caller specifies.
        let dir = tempfile::TempDir::new().unwrap();
        let archive_path = dir.path().join("a.tar.zst");
        let mut tar_buf: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let mut header = tar::Header::new_gnu();
            header.set_size(4);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "libcurl-8.20.0-aaaa/lib/libcurl.so.4", &b"data"[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let zst = zstd::encode_all(&tar_buf[..], 0).unwrap();
        std::fs::write(&archive_path, zst).unwrap();

        let into = dir.path().join("out");
        std::fs::create_dir_all(&into).unwrap();
        extract_tar_zst(&archive_path, &into, "libcurl-8.20.0-aaaa").unwrap();

        assert!(into.join("lib/libcurl.so.4").is_file());
        assert!(!into.join("libcurl-8.20.0-aaaa").exists());
    }

    // Uses `MetadataExt::ino()` to confirm a real hardlink (same inode)
    // was produced. The inode API only exists on Unix; on Windows we'd
    // need to compare via `GetFileInformationByHandle`. Skip on
    // Windows — the behavior the test covers (rewriting the link
    // target with `strip_prefix`) is exercised the same way on either
    // platform; the inode check is incidental.
    #[cfg(unix)]
    #[test]
    fn extract_rewrites_hardlink_targets_with_strip_prefix() {
        // Mirrors the rabbitmq tarball shape that previously broke
        // `services up`: a regular file at `install/escript/rabbitmq-
        // diagnostics` followed by several hardlinks whose tar header
        // records the link target with the `install/` prefix.
        // Without rewriting, the link target dangled because we only
        // wrote the file at `<into>/escript/rabbitmq-diagnostics`.
        let dir = tempfile::TempDir::new().unwrap();
        let archive_path = dir.path().join("rmq.tar.zst");

        let body = b"escript-stub";
        let mut tar_buf: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            // The original file.
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder
                .append_data(&mut header, "install/escript/rabbitmq-diagnostics", &body[..])
                .unwrap();
            // Hardlinks share the entry-type code with the regular
            // GNU header; size is zero and link_name carries the
            // source path (still prefixed by `install/` here).
            let mut link_header = tar::Header::new_gnu();
            link_header.set_size(0);
            link_header.set_mode(0o755);
            link_header.set_entry_type(tar::EntryType::Link);
            link_header
                .set_link_name("install/escript/rabbitmq-diagnostics")
                .unwrap();
            link_header.set_cksum();
            builder
                .append_data(&mut link_header, "install/escript/rabbitmqctl", std::io::empty())
                .unwrap();
            builder.finish().unwrap();
        }
        let zst = zstd::encode_all(&tar_buf[..], 0).unwrap();
        std::fs::write(&archive_path, zst).unwrap();

        let into = dir.path().join("out");
        std::fs::create_dir_all(&into).unwrap();
        extract_tar_zst(&archive_path, &into, "install").unwrap();

        let orig = into.join("escript/rabbitmq-diagnostics");
        let linked = into.join("escript/rabbitmqctl");
        assert!(orig.is_file());
        assert!(linked.is_file());
        // Same inode → same hardlink target, same contents.
        let m1 = std::fs::metadata(&orig).unwrap();
        let m2 = std::fs::metadata(&linked).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(m1.ino(), m2.ino());
        assert_eq!(std::fs::read(&linked).unwrap(), body);
    }

    /// windows.php.net's interpreter ZIP wraps `php.exe`, `ext/*.dll`,
    /// etc. inside a top-level `php-<version>/` directory. The extractor
    /// must strip that prefix so the materialized tree mirrors the
    /// `bin/`-style layout the rest of bougie assumes.
    #[test]
    fn extract_zip_strips_prefix_and_writes_nested_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let zip_path = dir.path().join("a.zip");

        let mut buf: Vec<u8> = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut zw = zip::ZipWriter::new(cursor);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("php-8.4.3/php.exe", opts).unwrap();
            zw.write_all(b"MZ exe stub").unwrap();
            zw.start_file("php-8.4.3/ext/php_curl.dll", opts).unwrap();
            zw.write_all(b"DLL stub").unwrap();
            zw.start_file("php-8.4.3/php.ini-development", opts).unwrap();
            zw.write_all(b"; ini\n").unwrap();
            zw.finish().unwrap();
        }
        std::fs::write(&zip_path, &buf).unwrap();

        let into = dir.path().join("out");
        std::fs::create_dir_all(&into).unwrap();
        extract_zip(&zip_path, &into, "php-8.4.3").unwrap();

        assert_eq!(std::fs::read(into.join("php.exe")).unwrap(), b"MZ exe stub");
        assert_eq!(std::fs::read(into.join("ext/php_curl.dll")).unwrap(), b"DLL stub");
        assert_eq!(std::fs::read(into.join("php.ini-development")).unwrap(), b"; ini\n");
        // The wrapping directory itself is not materialized.
        assert!(!into.join("php-8.4.3").exists());
    }

    /// PECL DLL ZIPs are flat — pass `strip_prefix = ""` and entries
    /// land verbatim.
    #[test]
    fn extract_zip_passes_through_when_no_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let zip_path = dir.path().join("flat.zip");
        let mut buf: Vec<u8> = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut zw = zip::ZipWriter::new(cursor);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("php_xdebug.dll", opts).unwrap();
            zw.write_all(b"xdebug").unwrap();
            zw.finish().unwrap();
        }
        std::fs::write(&zip_path, &buf).unwrap();

        let into = dir.path().join("out");
        std::fs::create_dir_all(&into).unwrap();
        extract_zip(&zip_path, &into, "").unwrap();

        assert_eq!(std::fs::read(into.join("php_xdebug.dll")).unwrap(), b"xdebug");
    }

    #[test]
    fn rewrite_archive_path_strips_when_prefixed() {
        let out = rewrite_archive_path(Path::new("install/bin/php"), "install").unwrap();
        assert_eq!(out, Path::new("bin/php"));
    }

    #[test]
    fn rewrite_archive_path_passes_through_when_unprefixed() {
        let out = rewrite_archive_path(Path::new("bin/php"), "install").unwrap();
        assert_eq!(out, Path::new("bin/php"));
    }

    #[test]
    fn rewrite_archive_path_handles_empty_prefix() {
        let out = rewrite_archive_path(Path::new("bin/php"), "").unwrap();
        assert_eq!(out, Path::new("bin/php"));
    }

    #[test]
    fn rewrite_archive_path_returns_none_for_prefix_directory_entry() {
        // The prefix dir entry itself (`install/`) rewrites to an
        // empty path. Caller skips the entry because the destination
        // root already exists.
        assert!(rewrite_archive_path(Path::new("install"), "install").is_none());
    }
}
