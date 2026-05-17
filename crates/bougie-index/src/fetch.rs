//! Index root fetcher with `ETag`/`If-None-Match` revalidation
//! per CLI.md §7.1.
//!
//! The cache layout under `$BOUGIE_CACHE/index/<host>/` carries:
//!   - `index.json`         the last-fetched root bytes
//!   - `index.json.etag`    the matching `ETag` header value
//!   - `index.json.sig`     the matching signature sidecar

use bougie_errors::BougieError;
use crate::verify::Verifier;
use crate::wire::{Manifest, Root, Section};
use eyre::{Result, WrapErr};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    /// 200 OK; the root was re-downloaded and re-verified.
    Refreshed,
    /// 304 Not Modified; the cached root was reused.
    Cached,
}

#[derive(Debug)]
pub struct FetchedRoot {
    pub root: Root,
    pub outcome: FetchOutcome,
}

/// Fetch (or revalidate) the root index. On success, the cache is up
/// to date and the parsed root is returned.
///
/// `build_verifier` is invoked lazily — only when the server returns a
/// fresh body that needs to be verified. On a 304 the cached signed
/// bytes are reused as-is and no verifier is constructed. This matters
/// for callers that pay a non-trivial cost for verifier construction
/// (e.g. the production Sigstore Bundle verifier walks the public-good
/// TUF trust root over the network).
pub fn fetch_root<F>(
    client: &reqwest::blocking::Client,
    host_base_url: &str,
    cache_root: &Path,
    build_verifier: F,
) -> Result<FetchedRoot>
where
    F: FnOnce() -> Result<Box<dyn Verifier>>,
{
    fs::create_dir_all(cache_root)
        .wrap_err_with(|| format!("creating {}", cache_root.display()))?;

    let root_path = cache_root.join("index.json");
    let etag_path = cache_root.join("index.json.etag");
    let sig_path = cache_root.join("index.json.sig");

    let cached_etag = fs::read_to_string(&etag_path).ok();
    let url = format!("{}/index.json", host_base_url.trim_end_matches('/'));

    let mut req = client.get(&url);
    if let Some(etag) = cached_etag.as_deref().filter(|s| !s.is_empty()) {
        req = req.header(reqwest::header::IF_NONE_MATCH, etag.trim());
    }
    let resp = req
        .send()
        .map_err(|e| net_io(format!("fetching {url}"), &e))?;

    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        let bytes = fs::read(&root_path)
            .wrap_err_with(|| format!("reading cached {}", root_path.display()))?;
        let root: Root = serde_json::from_slice(&bytes).wrap_err("parsing cached index.json")?;
        return Ok(FetchedRoot { root, outcome: FetchOutcome::Cached });
    }

    if !resp.status().is_success() {
        return Err(net_http(format!("GET {url}"), resp.status()).into());
    }
    let new_etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = resp
        .bytes()
        .map_err(|e| net_io(format!("reading body of {url}"), &e))?;

    let sig_url = format!("{url}.sig");
    let sig_resp = client
        .get(&sig_url)
        .send()
        .map_err(|e| net_io(format!("fetching {sig_url}"), &e))?;
    if !sig_resp.status().is_success() {
        return Err(net_http(format!("GET {sig_url}"), sig_resp.status()).into());
    }
    let sig_bytes = sig_resp
        .bytes()
        .map_err(|e| net_io(format!("reading signature body of {sig_url}"), &e))?;

    let verifier = build_verifier()?;
    verifier.verify(&url, &body, &sig_bytes)?;

    // Atomic writes (rename within cache_root, same FS).
    atomic_write(&root_path, &body)?;
    atomic_write(&sig_path, &sig_bytes)?;
    if let Some(etag) = new_etag.as_deref() {
        atomic_write(&etag_path, etag.as_bytes())?;
    }

    let root: Root = serde_json::from_slice(&body).wrap_err("parsing fetched index.json")?;
    Ok(FetchedRoot { root, outcome: FetchOutcome::Refreshed })
}

/// Fetch a section file, verifying its sha256 against the value the
/// signed root advertised. Cached files are reused when their hash
/// matches the expected sha; mismatches force a refetch.
///
/// The section URL embeds the root's `version` per DISTRIBUTION.md
/// §Snapshot-consistency, so each publish lands at a fresh immutable
/// URL and a CDN cache of the old URL never collides with new content.
pub fn fetch_section(
    client: &reqwest::blocking::Client,
    host_base_url: &str,
    cache_root: &Path,
    publish_version: &str,
    target: &str,
    section_name: &str,
    expected_sha256: &str,
) -> Result<Section> {
    // Cache by content sha — the URL changes per publish but the body
    // identity is the sha. Sharing the cache key across versions means
    // a section that didn't change between publishes is reused.
    let cache_path = cache_root
        .join("sections")
        .join(format!("{expected_sha256}.json"));
    if let Ok(bytes) = fs::read(&cache_path)
        && hex_sha256(&bytes) == expected_sha256
    {
        return serde_json::from_slice(&bytes).wrap_err("parsing cached section");
    }
    let url = format!(
        "{}/versions/{}/targets/{}/sections/{}.json",
        host_base_url.trim_end_matches('/'),
        publish_version,
        target,
        section_name
    );
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| net_io(format!("fetching {url}"), &e))?;
    if !resp.status().is_success() {
        return Err(net_http(format!("GET {url}"), resp.status()).into());
    }
    let body = resp
        .bytes()
        .map_err(|e| net_io(format!("reading body of {url}"), &e))?;
    let actual = hex_sha256(&body);
    if actual != expected_sha256 {
        return Err(BougieError::ManifestHashMismatch {
            url: url.clone(),
            expected: expected_sha256.to_owned(),
            actual,
        }
        .into());
    }
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).wrap_err("creating section cache dir")?;
    }
    atomic_write(&cache_path, &body)?;
    serde_json::from_slice(&body).wrap_err("parsing fetched section")
}

/// Fetch a manifest body, cached by sha256.
///
/// `manifest_path` is the server-absolute path the section row carries
/// (e.g. `/targets/<target>/manifests/...`). Hostname-relative paths
/// dodge a class of URL-resolution bugs that bit relative `../../...`
/// references when section names contained `/`. See DISTRIBUTION.md
/// §Manifests-and-blobs ("Why absolute manifest paths").
pub fn fetch_manifest(
    client: &reqwest::blocking::Client,
    host_base_url: &str,
    cache_root: &Path,
    manifest_path: &str,
    expected_sha256: &str,
) -> Result<Manifest> {
    let cache_path = cache_root.join("manifests").join(format!("{expected_sha256}.json"));
    if let Ok(bytes) = fs::read(&cache_path)
        && hex_sha256(&bytes) == expected_sha256
    {
        return serde_json::from_slice(&bytes).wrap_err("parsing cached manifest");
    }
    let url = format!(
        "{}{}",
        host_base_url.trim_end_matches('/'),
        // Path is server-absolute and starts with '/'. Defensive: a
        // publisher that forgets the leading slash still produces a
        // working URL via the format string below.
        if manifest_path.starts_with('/') {
            manifest_path.to_owned()
        } else {
            format!("/{manifest_path}")
        }
    );
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| net_io(format!("fetching manifest {url}"), &e))?;
    if !resp.status().is_success() {
        return Err(net_http(format!("GET {url}"), resp.status()).into());
    }
    let body = resp
        .bytes()
        .map_err(|e| net_io(format!("reading manifest body of {url}"), &e))?;
    let actual = hex_sha256(&body);
    if actual != expected_sha256 {
        return Err(BougieError::ManifestHashMismatch {
            url: url.clone(),
            expected: expected_sha256.to_owned(),
            actual,
        }
        .into());
    }
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent).wrap_err("creating manifest cache dir")?;
    }
    atomic_write(&cache_path, &body)?;
    serde_json::from_slice(&body).wrap_err("parsing fetched manifest")
}

fn net_io(operation: String, e: &impl std::fmt::Display) -> BougieError {
    BougieError::Network { operation, detail: e.to_string() }
}

fn net_http(operation: String, status: reqwest::StatusCode) -> BougieError {
    BougieError::Network {
        operation,
        detail: format!("server returned HTTP {status}"),
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn atomic_write(dest: &Path, bytes: &[u8]) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| eyre::eyre!("destination has no parent: {}", dest.display()))?;
    let mut tmp = PathBuf::from(parent);
    let stem = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("tmp");
    tmp.push(format!(".{stem}.partial"));
    fs::write(&tmp, bytes).wrap_err_with(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, dest).wrap_err_with(|| format!("rename → {}", dest.display()))?;
    Ok(())
}
