//! Packagist v2 metadata fetcher.
//!
//! Phase C first slice: pull `/p2/<vendor>/<name>.json` (and the
//! `~dev.json` companion for branch versions) from Packagist with
//! ETag-based conditional GET. Reuses the existing
//! `$BOUGIE_CACHE/composer-metadata/` directory and the
//! `bougie_composer::metadata` parser / minified-expander.
//!
//! Two specific design choices worth noting:
//!
//! - **Per-package cache files, not a single index.** Each
//!   `vendor/name.json` is independent on disk so two projects sharing
//!   `monolog/monolog` reuse the same cache entry across runs, and a
//!   second `composer update` only re-validates the packages whose
//!   server-side mtime advanced.
//!
//! - **`If-None-Match` + ETag sidecar.** Packagist's response sets
//!   both `ETag` and `Last-Modified`; we follow the existing
//!   `bougie_composer::fetch` convention (`fetch_channels`) and use
//!   ETag. A 304 short-circuits to the cached body without an extra
//!   round-trip.
//!
//! The prefetcher (background tokio task that warms metadata for
//! downstream packages while pubgrub works on the current one) lands
//! in a follow-up PR. This module is the sync substrate it'll wrap.

use bougie_composer::metadata::PackageMetadata;
use bougie_errors::BougieError;
use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_BASE_URL: &str = "https://repo.packagist.org";

/// Production base URL. Reads `BOUGIE_PACKAGIST_BASE_URL` for mirrors
/// / air-gapped installs / tests, defaulting to Packagist. Pass the
/// returned string into [`fetch_package_metadata`] explicitly rather
/// than relying on a global — keeps the fetcher pure and lets the
/// prefetcher hold one resolved URL per session.
pub fn base_url() -> String {
    std::env::var("BOUGIE_PACKAGIST_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into())
}

/// Build the default blocking HTTP client used by the metadata
/// fetcher. Gzip negotiation is enabled (Packagist serves multi-MB
/// JSON; transparent gzip cuts wire bytes by ~5×). The user-agent
/// matches what Composer itself sends.
pub fn build_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent("bougie-composer-resolver")
        .build()
        .map_err(|e| {
            BougieError::Network {
                operation: "building Packagist HTTP client".into(),
                detail: e.to_string(),
            }
            .into()
        })
}

/// Which of the two `/p2/` documents to fetch for a package.
///
/// Packagist publishes stable releases in `<name>.json` and any
/// `dev-*` / branch versions in `<name>~dev.json`. Most resolves only
/// need the first; the dev variant matters when a `composer.json`
/// pins `dev-main` (or similar) or when `minimum-stability` allows
/// branch installs.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Variant {
    Stable,
    Dev,
}

impl Variant {
    fn suffix(self) -> &'static str {
        match self {
            Variant::Stable => "",
            Variant::Dev => "~dev",
        }
    }
}

/// Fetch parsed metadata for one package. Performs conditional GET
/// against the on-disk cache; on a 304 the cached body is reused
/// without re-parsing the wire response.
///
/// `base_url` is the Packagist host without a trailing slash, e.g.
/// `"https://repo.packagist.org"`. Use [`base_url()`] to derive it
/// from env, or pass a mock server's URI in tests.
pub fn fetch_package_metadata(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    base_url: &str,
    package_name: &str,
    variant: Variant,
) -> Result<PackageMetadata> {
    let (json_path, etag_path) = cache_paths(paths, package_name, variant);
    if let Some(parent) = json_path.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }

    let url = format!(
        "{}/p2/{}{}.json",
        base_url.trim_end_matches('/'),
        package_name,
        variant.suffix(),
    );
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
        return read_cached(&json_path);
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

    write_atomic(&json_path, &bytes)?;
    if let Some(t) = etag {
        let _ = write_atomic(&etag_path, t.as_bytes());
    }

    PackageMetadata::parse(&bytes)
        .wrap_err_with(|| format!("parsing metadata for {package_name}"))
}

fn read_cached(json_path: &Path) -> Result<PackageMetadata> {
    let bytes = fs::read(json_path)
        .wrap_err_with(|| format!("reading cached metadata at {}", json_path.display()))?;
    PackageMetadata::parse(&bytes).wrap_err_with(|| {
        format!("re-parsing cached metadata at {}", json_path.display())
    })
}

/// Compute the on-disk cache locations for a package. Returns
/// `(json_path, etag_path)`. Exposed for tests + the prefetcher.
pub fn cache_paths(paths: &Paths, package_name: &str, variant: Variant) -> (PathBuf, PathBuf) {
    let root = paths.cache_composer_metadata().join("p2");
    let json = root.join(format!("{package_name}{}.json", variant.suffix()));
    let etag = root.join(format!("{package_name}{}.etag", variant.suffix()));
    (json, etag)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
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
mod tests;
