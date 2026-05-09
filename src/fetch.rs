//! Atomic blob fetch + extract per CLI.md §7.4.
//!
//! Pattern: stream into `$BOUGIE_CACHE/blobs/<sha256>.partial`, verify
//! sha256 while writing, extract into `<dest>.incoming` (sibling of
//! the final destination so the rename is on the same filesystem),
//! atomic-rename to `<dest>`, delete `tmp`.

use crate::errors::BougieError;
use eyre::{Result, WrapErr};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{copy, Read, Write};
use std::path::{Path, PathBuf};

const RETRY_BUDGET: u32 = 1;

#[derive(Debug, Clone)]
pub struct BlobSpec<'a> {
    pub url: &'a str,
    pub sha256: &'a str,
    pub partial_dir: &'a Path,
    pub dest: &'a Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobOutcome {
    AlreadyPresent,
    Downloaded,
}

/// Fetch + extract one blob. No-op if `dest` exists.
pub fn fetch_blob(client: &reqwest::blocking::Client, spec: &BlobSpec<'_>) -> Result<BlobOutcome> {
    if spec.dest.exists() {
        return Ok(BlobOutcome::AlreadyPresent);
    }
    fs::create_dir_all(spec.partial_dir)
        .wrap_err_with(|| format!("creating {}", spec.partial_dir.display()))?;

    let mut attempts = 0;
    loop {
        match try_once(client, spec) {
            Ok(()) => return Ok(BlobOutcome::Downloaded),
            Err(e) if attempts < RETRY_BUDGET => {
                attempts += 1;
                tracing::warn!(error = %e, attempt = attempts, "blob fetch failed; retrying");
            }
            Err(e) => return Err(e),
        }
    }
}

fn try_once(client: &reqwest::blocking::Client, spec: &BlobSpec<'_>) -> Result<()> {
    let tmp = spec.partial_dir.join(format!("{}.partial", spec.sha256));

    let mut resp = client
        .get(spec.url)
        .send()
        .map_err(|e| BougieError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(BougieError::Network(format!("GET {} → {}", spec.url, resp.status())).into());
    }

    let mut file = File::create(&tmp).wrap_err_with(|| format!("creating {}", tmp.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| BougieError::Network(format!("reading body: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .wrap_err_with(|| format!("writing {}", tmp.display()))?;
    }
    file.flush().wrap_err("flushing partial blob")?;

    let actual = format_hex(&hasher.finalize());
    if !actual.eq_ignore_ascii_case(spec.sha256) {
        let _ = fs::remove_file(&tmp);
        return Err(BougieError::BlobHashMismatch.into());
    }

    let incoming = sibling_with_suffix(spec.dest, ".incoming");
    let _ = fs::remove_dir_all(&incoming);
    fs::create_dir_all(&incoming)
        .wrap_err_with(|| format!("creating {}", incoming.display()))?;
    extract_tar_zst(&tmp, &incoming)?;

    fs::rename(&incoming, spec.dest)
        .wrap_err_with(|| format!("rename {} → {}", incoming.display(), spec.dest.display()))?;
    let _ = fs::remove_file(&tmp);
    Ok(())
}

fn extract_tar_zst(tar_zst: &Path, into: &Path) -> Result<()> {
    let f = File::open(tar_zst)
        .wrap_err_with(|| format!("opening {}", tar_zst.display()))?;
    let zd = zstd::stream::read::Decoder::new(f).wrap_err("zstd decoder")?;
    let mut archive = tar::Archive::new(zd);
    archive
        .unpack(into)
        .wrap_err_with(|| format!("unpacking into {}", into.display()))?;
    Ok(())
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
}
