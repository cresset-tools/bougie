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

use bougie_composer::lockfile::LockPackage;
use bougie_composer::metadata::PackageMetadata;
use bougie_errors::BougieError;
use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use std::collections::{BTreeMap, HashMap};
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
    /// Discovered Composer protocol. Populated by the orchestrator
    /// (via [`probe_protocol`]) before any per-package fetches.
    /// `None` means "not probed yet" — the fetcher falls back to
    /// the v2 path, which is the right behavior for Packagist and
    /// for any composer-type repo whose packages.json couldn't be
    /// reached.
    pub protocol: Option<RepoProtocol>,
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
        Self { url, cache_namespace, auth: None, protocol: None }
    }

    /// Attach auth credentials. Chainable on `Repo::from_url`.
    pub fn with_auth(mut self, auth: Option<AuthCredentials>) -> Self {
        self.auth = auth;
        self
    }

    /// Attach a discovered protocol. Chainable; called by the
    /// orchestrator after `probe_protocol` so the per-package
    /// fetcher knows whether to take the v2 `/p2/` path or the v1
    /// provider-lookup path.
    pub fn with_protocol(mut self, protocol: Option<RepoProtocol>) -> Self {
        self.protocol = protocol;
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
/// by reading the repo's root `packages.json`. The discriminant is
/// the presence of a `metadata-url` field:
///
/// - **v2** advertises `metadata-url` (typically `/p2/%package%.json`)
///   and serves per-package metadata as static files. Bougie's
///   original protocol; the `fetch_package_metadata*` functions
///   target it directly.
/// - **v1** has no `metadata-url`; instead it ships `providers-url`
///   (a template for per-package files) and `provider-includes` (a
///   list of "package listing" files that map every available
///   package name to a sha256 hash, which gets substituted into
///   `providers-url` to form the per-package URL). The
///   [`V1Discovery`] payload carries the fields needed to drive the
///   lookup.
///
/// Both protocols deserialize per-package metadata into the same
/// [`PackageMetadata`] shape so the resolver above can stay
/// protocol-agnostic.
#[derive(Debug, Clone)]
pub enum RepoProtocol {
    /// `metadata-url` present. Use [`fetch_package_metadata_optional`].
    V2,
    /// `metadata-url` absent. Use [`load_v1_provider_table`] +
    /// [`fetch_package_metadata_v1_optional`].
    V1(V1Discovery),
}

/// Discovery data for a Composer v1 repository, extracted from its
/// `packages.json`. Drives the two-step v1 lookup: load every
/// provider-include to learn each package's sha256, then substitute
/// into `providers_url` to fetch the per-package metadata.
#[derive(Debug, Clone)]
pub struct V1Discovery {
    /// Per-package URL template with `%package%` and `%hash%`
    /// placeholders. e.g. `/p/%package%$%hash%.json`. The `%hash%`
    /// value comes from the [`ProviderInclude`] that lists the
    /// package.
    pub providers_url: String,
    /// Provider-listing files. Each entry has a path template
    /// (typically with literal `%hash%` substituted from `sha256`
    /// to form the actual URL) and a sha256 of the listing's body.
    /// `bougie` loads every include eagerly because v1 gives no
    /// way to predict which include holds a given package without
    /// downloading and scanning.
    pub provider_includes: Vec<ProviderInclude>,
}

/// One entry in a v1 `provider-includes` map. The path template's
/// `%hash%` placeholder is replaced with `sha256` to form the
/// fetched URL; the body is content-addressed so a sha256-keyed
/// disk cache never needs invalidation.
#[derive(Debug, Clone)]
pub struct ProviderInclude {
    pub path_template: String,
    pub sha256: String,
}

/// Probe a repository's `packages.json` and classify it as v2 or v1.
/// For v1, parse the `providers-url` and `provider-includes` fields
/// into a [`V1Discovery`] for the caller to drive the lookup with.
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
    let start = std::time::Instant::now();
    let resp = req.send().map_err(|e| BougieError::Network {
        operation: format!("GET {url}"),
        detail: e.to_string(),
    })?;
    tracing::debug!(
        url = %url,
        status = %resp.status(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        phase = "probe",
        "GET",
    );
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
        return Ok(RepoProtocol::V2);
    }
    // No metadata-url → v1. Pull the providers-url + provider-includes
    // we'll need for the lookup. A v1 repo without providers-url is
    // structurally broken — surface the error rather than masking it.
    let providers_url = root
        .get("providers-url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            eyre::eyre!(
                "packages.json from {url} has no `metadata-url` (v2) and no \
                 `providers-url` (v1); cannot determine how to fetch package metadata",
            )
        })?
        .to_owned();
    let mut provider_includes: Vec<ProviderInclude> = Vec::new();
    if let Some(includes) = root.get("provider-includes").and_then(serde_json::Value::as_object) {
        for (path_template, entry) in includes {
            let sha256 = entry
                .get("sha256")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    eyre::eyre!(
                        "packages.json from {url}: provider-includes[{path_template}] \
                         is missing a `sha256` field",
                    )
                })?
                .to_owned();
            provider_includes.push(ProviderInclude {
                path_template: path_template.clone(),
                sha256,
            });
        }
    }
    Ok(RepoProtocol::V1(V1Discovery {
        providers_url,
        provider_includes,
    }))
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

    let start = std::time::Instant::now();
    let resp = req.send().map_err(|e| BougieError::Network {
        operation: format!("GET {url}"),
        detail: e.to_string(),
    })?;
    tracing::debug!(
        url = %url,
        status = %resp.status(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        package = %package_name,
        variant = ?variant,
        phase = "fetch",
        "GET",
    );

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

    let start = std::time::Instant::now();
    let resp = req.send().map_err(|e| BougieError::Network {
        operation: format!("GET {url}"),
        detail: e.to_string(),
    })?;
    tracing::debug!(
        url = %url,
        status = %resp.status(),
        elapsed_ms = start.elapsed().as_millis() as u64,
        package = %package_name,
        variant = ?variant,
        phase = "fetch_optional",
        "GET",
    );

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

// ===========================================================================
// Composer v1 protocol
// ---------------------------------------------------------------------------
// `provider-includes` + `providers-url` flow:
//
// 1. Probe `packages.json` (already done by `probe_protocol`) yields a
//    [`V1Discovery`] with the per-package URL template and the list of
//    provider-include files (each carrying a `sha256`).
// 2. [`load_v1_provider_table`] downloads every provider-include and
//    merges them into a single `package_name → sha256` lookup map. The
//    include files themselves are content-addressed (sha256 is in the
//    URL), so once cached on disk they never need re-fetching.
// 3. To resolve a single package, [`fetch_package_metadata_v1_optional`]
//    looks up the package's sha256 in the lookup table, substitutes
//    `%package%` + `%hash%` into `providers_url`, and fetches the
//    per-package JSON. That body is also content-addressed: once
//    written, never invalidated.
// 4. The per-package JSON is shaped
//    `{"packages": {"vendor/name": {"<version>": <entry>}}}` — versions
//    are an *object* keyed by version string, unlike the v2 *array*.
//    [`parse_v1_per_package`] converts to the same
//    [`PackageMetadata`] shape the v2 path produces, so callers stay
//    protocol-agnostic.
//
// What we deliberately skip in this first slice (track as follow-ups):
//
// - sha256 verification of downloaded bodies; we trust the host + TLS.
// - `providers-lazy-url` (alt v1 shape that skips `provider-includes`;
//   Drupal uses this — Magento doesn't).
// - `notify-batch` (post-install telemetry endpoint; irrelevant to
//   resolve).
// - Inline `packages` arrays at packages.json's top level (some satis
//   v1 builds put a small set of packages here; rare).

/// Lazily download every entry in `discovery.provider_includes`,
/// merge their `providers` maps into a single lookup table, and
/// return it. Includes are cached on disk under their `sha256`
/// (content-addressed → never invalidated).
///
/// When multiple includes list the same package name, later wins.
/// That matches Composer's own merge behavior.
pub fn load_v1_provider_table(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    repo: &Repo,
    discovery: &V1Discovery,
) -> Result<HashMap<String, String>> {
    let mut table: HashMap<String, String> = HashMap::new();
    for include in &discovery.provider_includes {
        let body = fetch_v1_include_cached(client, paths, repo, include)?;
        let parsed: serde_json::Value = serde_json::from_slice(&body)
            .wrap_err_with(|| {
                format!(
                    "parsing v1 provider-include from {} (sha256 {})",
                    include.path_template, include.sha256,
                )
            })?;
        let Some(providers) = parsed.get("providers").and_then(serde_json::Value::as_object)
        else {
            // A `provider-includes` file with no `providers` map is
            // structurally degenerate; treat as empty rather than
            // erroring so one bad include doesn't sink the whole resolve.
            continue;
        };
        for (name, entry) in providers {
            let Some(sha) = entry
                .get("sha256")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            table.insert(name.clone(), sha.to_owned());
        }
    }
    Ok(table)
}

/// Fetch one provider-include, using the on-disk content-addressed
/// cache when present. Substitutes `%hash%` in `path_template` with
/// the include's `sha256` to form the URL.
fn fetch_v1_include_cached(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    repo: &Repo,
    include: &ProviderInclude,
) -> Result<Vec<u8>> {
    let cache_path = v1_include_cache_path(paths, repo, &include.sha256);
    if let Ok(bytes) = fs::read(&cache_path) {
        return Ok(bytes);
    }
    let url_path = include.path_template.replace("%hash%", &include.sha256);
    let url = format!("{}/{}", repo.url, url_path.trim_start_matches('/'));
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
    write_atomic(&cache_path, &bytes)?;
    Ok(bytes.to_vec())
}

/// Per-package fetcher for v1 repos. Looks up `package_name` in the
/// pre-loaded provider table; if not present, returns `Ok(None)`
/// (the repo simply doesn't carry this package and the caller moves
/// to the next repo). Otherwise substitutes `%package%` + `%hash%`
/// into `providers_url`, fetches, and parses into the v2-equivalent
/// [`PackageMetadata`] shape so the resolver above can stay
/// protocol-agnostic.
///
/// The per-package response body is content-addressed (the URL
/// already contains the sha256 of the body), so on-disk caching
/// is unconditional and never invalidated.
pub fn fetch_package_metadata_v1_optional(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    repo: &Repo,
    discovery: &V1Discovery,
    provider_table: &HashMap<String, String>,
    package_name: &str,
) -> Result<Option<PackageMetadata>> {
    let Some(sha) = provider_table.get(package_name) else {
        return Ok(None);
    };
    let cache_path = v1_package_cache_path(paths, repo, package_name, sha);
    let bytes = if let Ok(bytes) = fs::read(&cache_path) {
        bytes
    } else {
        let url_path = discovery
            .providers_url
            .replace("%package%", package_name)
            .replace("%hash%", sha);
        let url = format!("{}/{}", repo.url, url_path.trim_start_matches('/'));
        let mut req = client.get(&url);
        if let Some(auth) = &repo.auth {
            req = req.header(reqwest::header::AUTHORIZATION, auth.header_value());
        }
        let resp = req.send().map_err(|e| BougieError::Network {
            operation: format!("GET {url}"),
            detail: e.to_string(),
        })?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // sha was in the listing but the file is missing — treat
            // like any other miss rather than crashing the resolve.
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(BougieError::Network {
                operation: format!("GET {url}"),
                detail: format!("server returned HTTP {}", resp.status()),
            }
            .into());
        }
        let body = resp.bytes().map_err(|e| BougieError::Network {
            operation: format!("reading body of {url}"),
            detail: e.to_string(),
        })?;
        write_atomic(&cache_path, &body)?;
        body.to_vec()
    };
    parse_v1_per_package(&bytes, package_name).map(Some)
}

/// Convert a v1 per-package response body
/// (`{"packages": {"vendor/name": {"<version>": {entry}}}}`) into
/// the same [`PackageMetadata`] the v2 minified-expansion path
/// produces. Versions are an object keyed by version string in v1,
/// vs. an array in v2 — beyond that the per-version entries reuse
/// the same Composer schema, so `LockPackage`'s deserializer (which
/// ignores unknown fields like Packagist's `uid`) accepts both
/// forms unchanged.
fn parse_v1_per_package(bytes: &[u8], package_name: &str) -> Result<PackageMetadata> {
    let root: serde_json::Value = serde_json::from_slice(bytes)
        .wrap_err_with(|| format!("parsing v1 per-package metadata for {package_name}"))?;
    let mut packages: BTreeMap<String, Vec<LockPackage>> = BTreeMap::new();
    let Some(pkgs) = root.get("packages").and_then(serde_json::Value::as_object) else {
        return Ok(PackageMetadata { packages });
    };
    for (name, versions) in pkgs {
        let Some(versions_obj) = versions.as_object() else {
            continue;
        };
        let mut list: Vec<LockPackage> = Vec::with_capacity(versions_obj.len());
        for (_version_key, entry) in versions_obj {
            let pkg: LockPackage = serde_json::from_value(entry.clone()).wrap_err_with(
                || {
                    format!(
                        "deserializing v1 per-package entry for {name} version {_version_key}",
                    )
                },
            )?;
            list.push(pkg);
        }
        packages.insert(name.clone(), list);
    }
    Ok(PackageMetadata { packages })
}

/// Cache path for one provider-include body, content-addressed by
/// sha256. Lives under the repo's namespace so two repos with the
/// same sha256 (theoretical but harmless) don't collide.
fn v1_include_cache_path(paths: &Paths, repo: &Repo, sha256: &str) -> PathBuf {
    paths
        .cache_composer_metadata()
        .join(&repo.cache_namespace)
        .join("p1")
        .join("includes")
        .join(format!("{sha256}.json"))
}

/// Cache path for one per-package body. The URL itself is
/// content-addressed (sha256 in the path), so the on-disk cache
/// mirrors the same key.
fn v1_package_cache_path(paths: &Paths, repo: &Repo, package_name: &str, sha256: &str) -> PathBuf {
    paths
        .cache_composer_metadata()
        .join(&repo.cache_namespace)
        .join("p1")
        .join("packages")
        .join(format!("{package_name}${sha256}.json"))
}

#[cfg(test)]
mod tests;
