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
/// / air-gapped installs / tests, defaulting to Packagist. Use
/// [`Repo::packagist`] to get a pre-built repo wrapping this URL.
pub fn base_url() -> String {
    std::env::var("BOUGIE_PACKAGIST_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into())
}

/// A Composer-protocol repository. Holds the base URL the resolver
/// fetches `/p2/<vendor>/<name>.json` from, plus a cache-namespace
/// string (typically the URL's host) so two repos at different hosts
/// can each cache a package with the same name without collision.
///
/// Authentication, custom `packages.json` metadata-url discovery,
/// non-Composer types (`vcs`, `path`, `package`, `artifact`) are
/// out of scope for this first repository slice — see the module
/// docs for follow-up status.
#[derive(Debug, Clone)]
pub struct Repo {
    pub url: String,
    /// Filesystem-safe cache namespace. For production this is the
    /// URL's host (e.g. `repo.packagist.org`). For tests, set
    /// explicitly so the wiremock-rooted URLs collide cleanly per
    /// scenario.
    pub cache_namespace: String,
}

impl Repo {
    /// Build a repo from a URL. The cache namespace is taken from
    /// the URL's host; for URLs without a host (e.g. `file:` URIs
    /// in tests), a hash-like fallback is used. Trailing slash on
    /// the URL is stripped.
    pub fn from_url(raw: impl Into<String>) -> Self {
        let url = raw.into().trim_end_matches('/').to_owned();
        let cache_namespace = extract_cache_namespace(&url);
        Self { url, cache_namespace }
    }

    /// Convenience for the implicit public Packagist repository,
    /// honoring `BOUGIE_PACKAGIST_BASE_URL` for tests / air-gapped
    /// installs / mirrors.
    pub fn packagist() -> Self {
        Self::from_url(base_url())
    }
}

/// Extract a filesystem-safe namespace from a repo URL. Strategy:
/// take the host portion (between `://` and the next `/` or end of
/// string). Falls back to a base64-flavored hash of the full URL
/// when no host is parseable (rare; mostly `file:` test URIs).
fn extract_cache_namespace(url: &str) -> String {
    if let Some(after_scheme) = url.split_once("://").map(|(_, rest)| rest) {
        let host = after_scheme
            .split('/')
            .next()
            .unwrap_or("")
            .split(':') // strip port
            .next()
            .unwrap_or("");
        if !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return host.to_owned();
        }
    }
    // Fallback: a short hex digest of the full URL. Use a cheap
    // FNV-style hash rather than pulling sha2 here.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in url.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("url-{h:016x}")
}

/// Build the default blocking HTTP client used by the metadata
/// fetcher. Routes through [`bougie_fetch::default_client`] so the
/// `User-Agent` matches every other bougie outbound request — gives
/// Packagist a single identifying string to rate-limit or contact
/// against if bougie ever misbehaves. Gzip negotiation is on for the
/// crate as a whole via the `gzip` feature flag (Packagist serves
/// multi-MB JSON; transparent gzip cuts wire bytes by ~5×).
pub fn build_client() -> Result<reqwest::blocking::Client> {
    bougie_fetch::default_client()
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

/// Fetch parsed metadata for one package from a specific repo.
/// Performs conditional GET against the on-disk cache; on a 304 the
/// cached body is reused without re-parsing the wire response.
pub fn fetch_package_metadata(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    repo: &Repo,
    package_name: &str,
    variant: Variant,
) -> Result<PackageMetadata> {
    let (json_path, etag_path) = cache_paths(paths, repo, package_name, variant);
    if let Some(parent) = json_path.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }

    let url = format!(
        "{}/p2/{}{}.json",
        repo.url,
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

/// Like [`fetch_package_metadata`] but returns `Ok(None)` when the
/// upstream replies 404. Useful for the `~dev.json` variant: many
/// packages have no branches, so Packagist 404s for them — the
/// resolver wants to treat that as "no dev candidates," not as a
/// hard failure.
///
/// Any other non-success status, parse failure, or transport error
/// still propagates as `Err`.
pub fn fetch_package_metadata_optional(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    repo: &Repo,
    package_name: &str,
    variant: Variant,
) -> Result<Option<PackageMetadata>> {
    let (json_path, etag_path) = cache_paths(paths, repo, package_name, variant);
    if let Some(parent) = json_path.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }

    let url = format!(
        "{}/p2/{}{}.json",
        repo.url,
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
        return read_cached(&json_path).map(Some);
    }
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
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
        .map(Some)
}

fn read_cached(json_path: &Path) -> Result<PackageMetadata> {
    let bytes = fs::read(json_path)
        .wrap_err_with(|| format!("reading cached metadata at {}", json_path.display()))?;
    PackageMetadata::parse(&bytes).wrap_err_with(|| {
        format!("re-parsing cached metadata at {}", json_path.display())
    })
}

/// Compute the on-disk cache locations for a package's metadata
/// from a specific repo. Returns `(json_path, etag_path)`. The
/// repo's `cache_namespace` segments the cache so the same package
/// name on different hosts doesn't collide. Exposed for tests + the
/// prefetcher.
pub fn cache_paths(
    paths: &Paths,
    repo: &Repo,
    package_name: &str,
    variant: Variant,
) -> (PathBuf, PathBuf) {
    let root = paths
        .cache_composer_metadata()
        .join(&repo.cache_namespace)
        .join("p2");
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
