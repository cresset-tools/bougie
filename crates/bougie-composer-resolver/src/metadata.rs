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
/// Custom `packages.json` metadata-url discovery and non-Composer
/// types (`vcs`, `path`, `package`, `artifact`) are still out of
/// scope — see the module docs for follow-up status.
#[derive(Debug, Clone)]
pub struct Repo {
    pub url: String,
    /// Filesystem-safe cache namespace. For production this is the
    /// URL's host (e.g. `repo.packagist.org`). For tests, set
    /// explicitly so the wiremock-rooted URLs collide cleanly per
    /// scenario.
    pub cache_namespace: String,
    /// Optional credentials to attach as the `Authorization` header
    /// on every request to this repo. Read from composer.json's
    /// `config.http-basic` / `config.bearer` (or a project-level
    /// `auth.json`) by `read_repository_auth`. Public Packagist
    /// is unauthenticated; private mirrors / satis usually need
    /// HTTP Basic, GitLab-CI Composer endpoints use Bearer.
    pub auth: Option<AuthCredentials>,
}

/// Auth credentials for a single repository. Skipped from `Debug`
/// output so credentials never leak into logs / error messages.
#[derive(Clone)]
pub enum AuthCredentials {
    /// HTTP Basic — Composer's `http-basic` shape.
    Basic { username: String, password: String },
    /// Bearer token — Composer's `bearer` shape.
    Bearer { token: String },
}

impl std::fmt::Debug for AuthCredentials {
    /// Redacted Debug. Crucial for not leaking creds into eyre
    /// chains or telemetry. The structural distinction (Basic vs
    /// Bearer) is fine to print; the actual secret material isn't.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Basic { username, .. } => {
                f.debug_struct("Basic")
                    .field("username", username)
                    .field("password", &"<redacted>")
                    .finish()
            }
            Self::Bearer { .. } => f.debug_struct("Bearer")
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

impl AuthCredentials {
    /// Render the credentials as an `Authorization` header value.
    /// For Basic, this is `Basic <base64(user:pass)>`; for Bearer,
    /// `Bearer <token>`.
    pub fn header_value(&self) -> String {
        use base64::Engine;
        match self {
            Self::Basic { username, password } => {
                let raw = format!("{username}:{password}");
                let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
                format!("Basic {encoded}")
            }
            Self::Bearer { token } => format!("Bearer {token}"),
        }
    }
}

impl Repo {
    /// Build a repo from a URL. The cache namespace is taken from
    /// the URL's host; for URLs without a host (e.g. `file:` URIs
    /// in tests), a hash-like fallback is used. Trailing slash on
    /// the URL is stripped. Auth defaults to `None`; attach via
    /// [`Repo::with_auth`] when registering against a host that
    /// requires credentials.
    pub fn from_url(raw: impl Into<String>) -> Self {
        let url = raw.into().trim_end_matches('/').to_owned();
        let cache_namespace = extract_cache_namespace(&url);
        Self { url, cache_namespace, auth: None }
    }

    /// Attach auth credentials. Chainable on `Repo::from_url`.
    pub fn with_auth(mut self, auth: Option<AuthCredentials>) -> Self {
        self.auth = auth;
        self
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

/// Which Composer-protocol version a repository speaks. Discovered
/// by reading the repo's root `packages.json`: v2 servers advertise
/// a `metadata-url` template (Composer's "metadata as static files"
/// shape), v1 servers don't — they use `provider-includes` /
/// `providers-url` for lazy provider discovery instead.
///
/// bougie only implements the v2 protocol today. The probe exists
/// so v1 repos can be detected and skipped cleanly (with a one-shot
/// warning) instead of crashing on a 302→HTML response to a `/p2/`
/// request. See [`probe_protocol`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RepoProtocol {
    /// `metadata-url` present in `packages.json`. Resolver fetches
    /// `/p2/<name>.json` directly.
    V2,
    /// `metadata-url` absent. Repository serves the v1 lazy-provider
    /// protocol, which bougie doesn't implement yet. Such repos are
    /// dropped from the working list at solve time.
    V1,
}

/// Probe a repository's protocol version by GETting `<url>/packages.json`
/// and looking for a top-level `metadata-url` string field.
///
/// Returns `Err` on transport failure, non-success HTTP status, or
/// malformed JSON — the caller decides whether a probe failure
/// should disqualify the repo or leave it in place. (Today: leave
/// in place, so a transient packages.json hiccup doesn't silently
/// drop a working v2 repo.)
pub fn probe_protocol(
    client: &reqwest::blocking::Client,
    repo: &Repo,
) -> Result<RepoProtocol> {
    let url = format!("{}/packages.json", repo.url);
    let mut req = client.get(&url);
    if let Some(auth) = &repo.auth {
        req = req.header(reqwest::header::AUTHORIZATION, auth.header_value());
    }
    let resp = req.send().map_err(|e| BougieError::Network {
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
    let bytes = resp.bytes().map_err(|e| BougieError::Network {
        operation: format!("reading body of {url}"),
        detail: e.to_string(),
    })?;
    let root: serde_json::Value = serde_json::from_slice(&bytes)
        .wrap_err_with(|| format!("parsing packages.json from {url}"))?;
    if root
        .get("metadata-url")
        .and_then(serde_json::Value::as_str)
        .is_some()
    {
        Ok(RepoProtocol::V2)
    } else {
        Ok(RepoProtocol::V1)
    }
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
    if let Some(auth) = &repo.auth {
        req = req.header(reqwest::header::AUTHORIZATION, auth.header_value());
    }
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
/// upstream replies 404 — or when the response body isn't JSON.
/// Useful for two distinct cases:
///
/// - The `~dev.json` variant: many packages have no branches, so
///   Packagist 404s for them; treat that as "no dev candidates," not
///   a hard failure.
/// - Composer v1 repos (e.g. `repo.magento.com`) that don't serve
///   `/p2/<name>.json` at all and instead return a 302 to a marketing
///   page — reqwest follows the redirect, the final response is a
///   200 `text/html` body, and `PackageMetadata::parse` then chokes
///   on "expected value at line 1 column 1." A non-JSON Content-Type
///   is the signal that this repo doesn't actually host `<name>`; the
///   resolver moves on to the next repo. The bogus body is *not*
///   written to the on-disk cache.
///
/// Any other non-success status, parse failure on an
/// `application/json` body, or transport error still propagates as
/// `Err`.
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
    if let Some(auth) = &repo.auth {
        req = req.header(reqwest::header::AUTHORIZATION, auth.header_value());
    }
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

    // 2xx HTML → treat as a repo miss. Stops bogus bodies (HTML from
    // redirect-to-marketing-site Composer v1 repos, sign-in pages
    // behind a transparent proxy, etc.) from crashing the resolve
    // *and* from poisoning the on-disk metadata cache. We deliberately
    // reject only the obviously-wrong types here: a `text/plain` or
    // missing Content-Type might still be valid JSON from a
    // misconfigured server, and we'd rather attempt the parse and
    // surface a clear error than silently de-list a working repo.
    if response_is_html(&resp) {
        return Ok(None);
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

/// True when a response's `Content-Type` advertises HTML — the
/// canonical "this is not your Composer metadata" shape. Used to
/// short-circuit a `/p2/` request that landed on a redirect-to-
/// marketing-site (the Magento v1 repo case) without poisoning the
/// JSON cache. Deliberately narrow: anything *not* HTML is left to
/// `PackageMetadata::parse` so a misconfigured server that ships
/// JSON as `text/plain` (or omits the header) still works.
fn response_is_html(resp: &reqwest::blocking::Response) -> bool {
    let Some(raw) = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let mime = raw.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    mime == "text/html" || mime == "application/xhtml+xml"
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
