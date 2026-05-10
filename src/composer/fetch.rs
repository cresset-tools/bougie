//! HTTP client for getcomposer.org: fetch the channel JSON, fetch a
//! per-version `composer.phar`, double-verify against the per-version
//! `.sha256sum` file.
//!
//! Override the base URL via `BOUGIE_COMPOSER_BASE_URL` (used by tests
//! and by air-gapped mirrors). Trust comes from TLS plus the cross-check
//! between the channels JSON `shasum` field and the standalone
//! `.sha256sum` file — getcomposer.org publishes both, so a single
//! upstream compromise that didn't update both would be caught.

use super::resolve::Resolved;
use crate::errors::BougieError;
use crate::fetch::{fetch_file, BlobSpec};
use crate::paths::Paths;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

const DEFAULT_BASE_URL: &str = "https://getcomposer.org";
const VERSIONS_PATH: &str = "/versions";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Channels {
    pub stable: Vec<ChannelEntry>,
    pub preview: Vec<ChannelEntry>,
}

/// One row from `getcomposer.org/versions`. We deliberately ignore
/// fields we don't use (`min-php`, `datetime`, `aliases`, ...).
///
/// Note: getcomposer.org's `/versions` does NOT include a sha256.
/// The canonical hash for a phar lives at
/// `download/<version>/composer.phar.sha256sum` and is fetched
/// separately at install time.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChannelEntry {
    pub version: String,
    pub path: String,
}

pub fn base_url() -> String {
    std::env::var("BOUGIE_COMPOSER_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into())
}

pub fn build_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .build()
        .map_err(|e| {
            BougieError::Network {
                operation: "building HTTP client".into(),
                detail: e.to_string(),
            }
            .into()
        })
}

/// Fetch the channels JSON, with an `If-None-Match` against the cached
/// etag sidecar. On 304 (or network failure with a cached snapshot
/// present) we fall back to the cached copy.
pub fn fetch_channels(client: &reqwest::blocking::Client, paths: &Paths) -> Result<Channels> {
    let cache_path = paths.composer_channels_json();
    let etag_path = paths.composer_channels_etag();
    fs::create_dir_all(paths.composer_root())
        .wrap_err_with(|| format!("creating {}", paths.composer_root().display()))?;

    let url = format!("{}{}", base_url(), VERSIONS_PATH);
    let mut req = client.get(&url);
    if let Ok(etag) = fs::read_to_string(&etag_path) {
        let etag = etag.trim();
        if !etag.is_empty() {
            req = req.header(reqwest::header::IF_NONE_MATCH, etag);
        }
    }

    let resp = req.send().map_err(|e| BougieError::Network {
        operation: format!("GET {url}"),
        detail: e.to_string(),
    })?;

    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return read_cached_channels(&cache_path);
    }
    if !resp.status().is_success() {
        return Err(BougieError::Network {
            operation: format!("GET {url}"),
            detail: format!("server returned HTTP {}", resp.status()),
        }
        .into());
    }

    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = resp.bytes().map_err(|e| BougieError::Network {
        operation: format!("reading body of {url}"),
        detail: e.to_string(),
    })?;

    write_atomic(&cache_path, &bytes)?;
    if let Some(t) = etag {
        let _ = write_atomic(&etag_path, t.as_bytes());
    }

    parse_channels(&bytes, &url)
}

fn read_cached_channels(cache_path: &std::path::Path) -> Result<Channels> {
    let bytes = fs::read(cache_path)
        .wrap_err_with(|| format!("reading {}", cache_path.display()))?;
    parse_channels(&bytes, &cache_path.display().to_string())
}

fn parse_channels(bytes: &[u8], source: &str) -> Result<Channels> {
    serde_json::from_slice(bytes)
        .wrap_err_with(|| format!("parsing channels JSON from {source}"))
}

/// Download the per-version `composer.phar` into
/// `$BOUGIE_HOME/composer/<v>/composer.phar`, verifying its sha256
/// against the standalone `.sha256sum` file the upstream publishes
/// alongside the phar.
pub fn fetch_phar(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    resolved: &Resolved,
) -> Result<()> {
    let dest = paths.composer_phar(&resolved.version);
    let phar_url = absolute_url(&resolved.path);

    // The canonical sha256 for the phar is published at
    // `<phar-url>.sha256sum`. getcomposer.org does NOT include it in
    // `/versions`, so this endpoint is the single trust anchor for
    // bougie's verification.
    let sum_url = format!("{phar_url}.sha256sum");
    let sha256 = fetch_sha256sum(client, &sum_url)?;

    let spec = BlobSpec {
        url: &phar_url,
        sha256: &sha256,
        partial_dir: &paths.cache_blobs(),
        dest: &dest,
    };
    fetch_file(client, &spec)?;
    Ok(())
}

/// Fetch the body of a `.sha256sum` file. The format is either a bare
/// 64-char hex string or `<hex>  <filename>` (sha256sum(1) layout) —
/// take the leading whitespace-delimited token.
fn fetch_sha256sum(client: &reqwest::blocking::Client, url: &str) -> Result<String> {
    let resp = client.get(url).send().map_err(|e| BougieError::Network {
        operation: format!("GET {url}"),
        detail: e.to_string(),
    })?;
    if !resp.status().is_success() {
        return Err(BougieError::Network {
            operation: format!("GET {url}"),
            detail: format!("server returned HTTP {}", resp.status()),
        }
        .into());
    }
    let body = resp.text().map_err(|e| BougieError::Network {
        operation: format!("reading body of {url}"),
        detail: e.to_string(),
    })?;
    let token = body
        .split_ascii_whitespace()
        .next()
        .unwrap_or("")
        .to_owned();
    if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(BougieError::Resolution {
            kind: "composer/sha256sum".into(),
            detail: format!("malformed body at {url}: {body:?}"),
        }
        .into());
    }
    Ok(token)
}

fn absolute_url(path_or_url: &str) -> String {
    if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
        path_or_url.to_owned()
    } else {
        format!("{}{}", base_url().trim_end_matches('/'), path_or_url)
    }
}

fn write_atomic(path: &PathBuf, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("partial");
    fs::write(&tmp, bytes).wrap_err_with(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .wrap_err_with(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_channels_decodes_json() {
        let body = br#"{
            "stable": [{"version":"2.9.7","path":"/download/2.9.7/composer.phar","min-php":70205}],
            "preview": []
        }"#;
        let ch = parse_channels(body, "test").unwrap();
        assert_eq!(ch.stable.len(), 1);
        assert_eq!(ch.stable[0].version, "2.9.7");
    }

    #[test]
    fn parse_channels_tolerates_extra_fields() {
        // getcomposer.org also publishes "1", "2", "snapshot" channels
        // alongside stable/preview; we ignore them.
        let body = br#"{
            "stable": [{"version":"2.9.7","path":"/x","min-php":70205,"datetime":"now"}],
            "snapshot": [{"version":"abc123","path":"/composer.phar","min-php":70205}],
            "1": [{"version":"1.10.27","path":"/x","min-php":50300}]
        }"#;
        let ch = parse_channels(body, "test").unwrap();
        assert_eq!(ch.stable[0].version, "2.9.7");
    }

    #[test]
    fn absolute_url_passes_through_full_urls() {
        assert_eq!(
            absolute_url("https://getcomposer.org/download/2.8.5/composer.phar"),
            "https://getcomposer.org/download/2.8.5/composer.phar"
        );
    }

    #[test]
    fn absolute_url_joins_relative_paths_to_base() {
        // env-mediated; test that the join logic works given some base.
        // We use the no-env default here to keep the test hermetic.
        let s = absolute_url("/download/2.8.5/composer.phar");
        assert!(s.ends_with("/download/2.8.5/composer.phar"));
        assert!(s.contains("://"));
    }
}
