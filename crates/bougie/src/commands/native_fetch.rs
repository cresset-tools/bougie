//! Shared machinery for fetching prebuilt native binaries from a
//! mirror→GitHub release URL pair: sequential-fallback download,
//! `.sha256` sidecar parsing, digest verification, tar.gz/zip
//! extraction, and atomic placement into a cache.
//!
//! Used by `bougie format` (its pinned wick download) and by the
//! `tool_callbacks::native_prefetcher` that warms launcher caches for
//! tools declaring `extra.bougie.native-binary`.

use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::path::Path;
use std::time::Duration;

use eyre::{eyre, Result, WrapErr};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

#[allow(clippy::duration_suboptimal_units)]
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Try each URL in order; succeed on the first that downloads.
pub fn download(client: &reqwest::blocking::Client, urls: &[String], dest: &Path) -> Result<()> {
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
pub fn parse_sidecar(path: &Path, archive_name: &str) -> Result<String> {
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

pub fn verify_sha256(file: &Path, expected: &str) -> Result<()> {
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

/// Extract a release archive. Both extractors reject entries that
/// would escape `into`.
pub fn extract(archive: &Path, into: &Path, zip: bool) -> Result<()> {
    let file =
        fs::File::open(archive).wrap_err_with(|| format!("opening {}", archive.display()))?;
    if zip {
        let mut zip = zip::ZipArchive::new(file).wrap_err("opening zip archive")?;
        zip.extract(into).wrap_err("extracting zip archive")?;
    } else {
        let dec = GzDecoder::new(file);
        let mut ar = tar::Archive::new(dec);
        ar.unpack(into).wrap_err("extracting tar.gz archive")?;
    }
    Ok(())
}

/// Move a staged binary into its final cache slot: copy to a sibling
/// `.partial` in the destination directory, mark executable, then
/// rename over the target so a concurrent reader never sees a
/// half-written file.
pub fn install_file_atomic(staged: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    let staging = dest.with_extension("partial");
    fs::copy(staged, &staging)
        .wrap_err_with(|| format!("staging {} into cache", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755))
            .wrap_err("marking binary executable")?;
    }
    fs::rename(&staging, dest)
        .wrap_err_with(|| format!("installing {} into cache", dest.display()))?;
    Ok(())
}
