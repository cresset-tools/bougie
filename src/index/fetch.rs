//! Index root fetcher with `ETag`/`If-None-Match` revalidation
//! per CLI.md §7.1.
//!
//! The cache layout under `$BOUGIE_CACHE/index/<host>/` carries:
//!   - `index.json`         the last-fetched root bytes
//!   - `index.json.etag`    the matching `ETag` header value
//!   - `index.json.sig`     the matching signature sidecar

use crate::errors::BougieError;
use crate::index::verify::Verifier;
use crate::index::wire::Root;
use eyre::{Result, WrapErr};
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
pub fn fetch_root(
    client: &reqwest::blocking::Client,
    host_base_url: &str,
    cache_root: &Path,
    verifier: &dyn Verifier,
) -> Result<FetchedRoot> {
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
    let resp = req.send().map_err(|e| BougieError::Network(e.to_string()))?;

    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        let bytes = fs::read(&root_path)
            .wrap_err_with(|| format!("reading cached {}", root_path.display()))?;
        let root: Root = serde_json::from_slice(&bytes).wrap_err("parsing cached index.json")?;
        return Ok(FetchedRoot { root, outcome: FetchOutcome::Cached });
    }

    if !resp.status().is_success() {
        return Err(BougieError::Network(format!("GET {} → {}", url, resp.status())).into());
    }
    let new_etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = resp
        .bytes()
        .map_err(|e| BougieError::Network(format!("reading body: {e}")))?;

    // Companion signature.
    let sig_url = format!("{url}.sig");
    let sig_resp = client
        .get(&sig_url)
        .send()
        .map_err(|e| BougieError::Network(e.to_string()))?;
    if !sig_resp.status().is_success() {
        return Err(BougieError::Network(format!(
            "GET {} → {}",
            sig_url,
            sig_resp.status()
        ))
        .into());
    }
    let sig_bytes = sig_resp
        .bytes()
        .map_err(|e| BougieError::Network(format!("reading signature: {e}")))?;

    verifier.verify(&body, &sig_bytes)?;

    // Atomic writes (rename within cache_root, same FS).
    atomic_write(&root_path, &body)?;
    atomic_write(&sig_path, &sig_bytes)?;
    if let Some(etag) = new_etag.as_deref() {
        atomic_write(&etag_path, etag.as_bytes())?;
    }

    let root: Root = serde_json::from_slice(&body).wrap_err("parsing fetched index.json")?;
    Ok(FetchedRoot { root, outcome: FetchOutcome::Refreshed })
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
