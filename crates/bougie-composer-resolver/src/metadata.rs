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
//! - **`If-None-Match` + `ETag` sidecar.** Packagist's response sets
//!   both `ETag` and `Last-Modified`; we follow the existing
//!   `bougie_composer::fetch` convention (`fetch_channels`) and use
//!   `ETag`. A 304 short-circuits to the cached body without an extra
//!   round-trip.
//!
//! The prefetcher (background tokio task that warms metadata for
//! downstream packages while pubgrub works on the current one) lands
//! in a follow-up PR. This module is the sync substrate it'll wrap.

use bougie_composer::lockfile::{DistMirror, LockPackage};
use bougie_composer::metadata::PackageMetadata;
use bougie_errors::{error_chain, BougieError};
use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use std::collections::BTreeMap;

use crate::hash::FxHashMap;
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
/// Custom `packages.json` metadata-url discovery and the remaining
/// non-Composer types (`vcs`, `package`, `artifact`) are still out of
/// scope — see the module docs for follow-up status. `path`
/// repositories are modeled via [`RepoKind::Path`].
#[derive(Debug, Clone)]
pub struct Repo {
    /// What kind of repository this is. Composer-protocol repos fetch
    /// `/p2/` metadata over HTTP from [`Repo::url`]; `path` repos
    /// glob a local directory tree and read each package's
    /// `composer.json` directly (no network).
    pub kind: RepoKind,
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
    /// Dist-mirror URL templates from the repo's root `packages.json`
    /// `mirrors` key (Private Packagist, satis with archive
    /// mirroring). Discovered by [`probe_protocol`] alongside the
    /// protocol; stamped onto every fetched package's `dist.mirrors`
    /// so they flow through resolution into the lockfile, matching
    /// Composer's `ComposerRepository::setDistMirrors` behavior.
    /// Empty for public Packagist and unprobed repos.
    pub dist_mirrors: Vec<DistMirror>,
}

/// What flavor of repository a [`Repo`] is.
///
/// `Composer` is the HTTP `/p2/`-fetching default (public Packagist,
/// satis, private mirrors). `Path` is a Composer `type: path`
/// repository: a glob over a local directory tree where each matched
/// directory's own `composer.json` is the package definition. Path
/// repos never hit the network — their candidates are seeded into the
/// resolver cache from disk before the solve.
#[derive(Debug, Clone)]
pub enum RepoKind {
    Composer,
    Path(PathRepoConfig),
    /// A Composer `type: vcs` (git) repository. Its candidates are
    /// discovered by cloning the repo and reading each tag/branch's
    /// `composer.json`, then seeded into the resolver cache before the
    /// solve — like [`RepoKind::Path`], never HTTP-fetched.
    Vcs(VcsRepoConfig),
}

/// Parsed configuration of a Composer `type: vcs` (git) repository entry.
/// Only the clone `url` is needed — versions come from the repo's git
/// tags and branches, discovered at seed time.
#[derive(Debug, Clone)]
pub struct VcsRepoConfig {
    /// The repository `url` verbatim from composer.json — a git remote
    /// (https or ssh) that `git` can clone.
    pub url: String,
}

/// Parsed configuration of a Composer `type: path` repository entry.
///
/// Holds the raw `url` (a path, possibly with `*`/`?` glob wildcards,
/// `~`, or env vars — expanded against the project root at seed time)
/// and the `options` block. The matched package directories are *not*
/// stored here; they're discovered when the resolver seeds path
/// candidates, because globbing needs the project root as a base.
#[derive(Debug, Clone)]
pub struct PathRepoConfig {
    /// The repository `url` verbatim from composer.json — a filesystem
    /// path or glob, relative to the project root unless absolute.
    pub url: String,
    /// `options.symlink`: `Some(true)` forces a symlink, `Some(false)`
    /// forces a copy, `None` is Composer's default (symlink, falling
    /// back to copy when symlinking fails).
    pub symlink: Option<bool>,
    /// `options.relative`: whether the symlink target is relative.
    /// `None` means unset — Composer's install-time default is
    /// *relative* (`true`), but the lock only records the key when the
    /// user set it explicitly, so this stays `Option`.
    pub relative: Option<bool>,
    /// `options.reference`: how the locked dist `reference` is derived.
    pub reference: ReferenceMode,
    /// `options.versions`: explicit per-package version overrides,
    /// keyed by package name. Wins over a `version` field inferred
    /// from the package's git state but loses to an explicit `version`
    /// in the package's own composer.json.
    pub versions: FxHashMap<String, String>,
}

/// Composer's `options.reference` for a path repository — controls
/// what goes in the locked dist's `reference` field, trading lockfile
/// churn against reproducibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceMode {
    /// Default: the git HEAD commit of the package dir when it is a
    /// git repository, otherwise a hash of its `composer.json` + repo
    /// config.
    Auto,
    /// Always the config hash, ignoring git — stable across commits in
    /// the package dir.
    Config,
    /// Always `null` — the lowest-churn choice.
    None,
}

/// Auth credentials for a single repository. Skipped from `Debug`
/// output so credentials never leak into logs / error messages.
#[derive(Clone)]
pub enum AuthCredentials {
    /// HTTP Basic — Composer's `http-basic` shape.
    Basic { username: String, password: String },
    /// Bearer token — Composer's `bearer` shape.
    Bearer { token: String },
    /// GitHub OAuth — sends `Authorization: token <tok>`.
    /// Composer only sends this to `api.github.com` URLs; for
    /// other `github.com` URLs the token is omitted.
    GitHubToken { token: String },
    /// GitLab private/CI token — sends `PRIVATE-TOKEN: <tok>`.
    GitLabToken { token: String },
}

impl std::fmt::Debug for AuthCredentials {
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
            Self::GitHubToken { .. } => f.debug_struct("GitHubToken")
                .field("token", &"<redacted>")
                .finish(),
            Self::GitLabToken { .. } => f.debug_struct("GitLabToken")
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

impl AuthCredentials {
    /// Render the credentials as an `Authorization` header value.
    pub fn header_value(&self) -> String {
        use base64::Engine;
        match self {
            Self::Basic { username, password } => {
                let raw = format!("{username}:{password}");
                let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
                format!("Basic {encoded}")
            }
            Self::Bearer { token } => format!("Bearer {token}"),
            Self::GitHubToken { token } => format!("token {token}"),
            Self::GitLabToken { token } => token.clone(),
        }
    }

    /// The HTTP header name for this credential type.
    pub fn header_name(&self) -> &'static str {
        match self {
            Self::Basic { .. } | Self::Bearer { .. } | Self::GitHubToken { .. } => "authorization",
            Self::GitLabToken { .. } => "private-token",
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
        Self {
            kind: RepoKind::Composer,
            url,
            cache_namespace,
            auth: None,
            protocol: None,
            dist_mirrors: Vec::new(),
        }
    }

    /// Build a `path` repository from its parsed config. The repo's
    /// `url` mirrors the raw path/glob (used for diagnostics and the
    /// cache namespace); resolution to concrete package directories
    /// happens later, against the project root. The cache namespace is
    /// a stable hash of the raw url so two path repos can't collide.
    pub fn path(config: PathRepoConfig) -> Self {
        let url = config.url.clone();
        let cache_namespace = format!("path-{:016x}", fnv1a(&url));
        Self {
            kind: RepoKind::Path(config),
            url,
            cache_namespace,
            auth: None,
            protocol: None,
            dist_mirrors: Vec::new(),
        }
    }

    /// Build a `vcs` (git) repository from its parsed config. Like
    /// [`Repo::path`], resolution to concrete package versions happens
    /// later (at seed time, by cloning and reading git refs); the cache
    /// namespace is a stable hash of the url.
    pub fn vcs(config: VcsRepoConfig) -> Self {
        let url = config.url.clone();
        let cache_namespace = format!("vcs-{:016x}", fnv1a(&url));
        Self {
            kind: RepoKind::Vcs(config),
            url,
            cache_namespace,
            auth: None,
            protocol: None,
            dist_mirrors: Vec::new(),
        }
    }

    /// Whether this is a `type: path` repository.
    pub fn is_path(&self) -> bool {
        matches!(self.kind, RepoKind::Path(_))
    }

    /// Whether this is a `type: vcs` (git) repository.
    pub fn is_vcs(&self) -> bool {
        matches!(self.kind, RepoKind::Vcs(_))
    }

    /// Whether this repo's candidates are seeded into the resolver cache
    /// locally (path from disk, vcs from a git clone) rather than fetched
    /// over HTTP. The metadata/prefetch paths skip these repos.
    pub fn seeds_candidates(&self) -> bool {
        matches!(self.kind, RepoKind::Path(_) | RepoKind::Vcs(_))
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

    /// Attach the dist-mirror templates discovered by
    /// [`probe_protocol`]. Chainable, set alongside `with_protocol`.
    pub fn with_dist_mirrors(mut self, dist_mirrors: Vec<DistMirror>) -> Self {
        self.dist_mirrors = dist_mirrors;
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
    format!("url-{:016x}", fnv1a(url))
}

/// The credential-lookup key for a repository URL — Composer's
/// `Url::getOrigin()`: the URL's host plus an explicit `:port` suffix
/// when the URL carries one (`127.0.0.1:8080`, `repo.example.com`).
///
/// This is what `auth.json` / `config.http-basic` / `COMPOSER_AUTH`
/// key credentials by, so — unlike [`extract_cache_namespace`], which
/// strips the port to produce a filesystem-safe cache directory name —
/// the port MUST be preserved here. Stripping it means basic/bearer
/// credentials for a mirror on a non-default port (a common shape for
/// local satis instances, e.g. `http://127.0.0.1:8080/...`) never
/// match their `auth.json` entry and every request 401s.
///
/// Falls back to the whole URL when no host can be parsed; that won't
/// match any host-keyed auth map, which is the correct "no
/// credentials" outcome.
pub fn auth_origin(url: &str) -> String {
    if let Some(after_scheme) = url.split_once("://").map(|(_, rest)| rest) {
        let host_and_port = after_scheme.split('/').next().unwrap_or("");
        if !host_and_port.is_empty() {
            return host_and_port.to_owned();
        }
    }
    url.to_owned()
}

/// FNV-1a 64-bit hash of a string. Cheap, allocation-free; used for
/// filesystem-safe cache namespaces where a real digest is overkill.
fn fnv1a(s: &str) -> u64 {
    // FNV-1a 64-bit offset basis and prime.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
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
/// Also extracts the root-level `mirrors` dist-url templates (second
/// tuple element) — Private Packagist and mirroring satis builds
/// declare them so dists download from the repo host (where the
/// project has credentials) instead of the origin VCS host (where it
/// usually doesn't; GitLab answers those unauthenticated archive GETs
/// with 404). Both protocol versions can carry the key.
///
/// Returns `Err` on transport failure, non-success HTTP status, or
/// malformed JSON — the caller decides whether a probe failure
/// should disqualify the repo or leave it in place. (Today: leave
/// in place, so a transient packages.json hiccup doesn't silently
/// drop a working v2 repo.)
pub fn probe_protocol(
    client: &reqwest::blocking::Client,
    repo: &Repo,
) -> Result<(RepoProtocol, Vec<DistMirror>)> {
    let url = format!("{}/packages.json", repo.url);
    let mut req = client.get(&url);
    if let Some(auth) = &repo.auth {
        req = req.header(auth.header_name(), auth.header_value());
    }
    let start = std::time::Instant::now();
    let resp = req.send().map_err(|e| BougieError::Network {
        operation: format!("GET {url}"),
        detail: error_chain(&e),
    })?;
    tracing::debug!(
        url = %url,
        status = %resp.status(),
        elapsed_ms = crate::elapsed_ms(start.elapsed()),
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
        detail: error_chain(&e),
    })?;
    let root: serde_json::Value = serde_json::from_slice(&bytes)
        .wrap_err_with(|| format!("parsing packages.json from {url}"))?;
    let dist_mirrors = parse_dist_mirrors(&root, &repo.url);
    if root
        .get("metadata-url")
        .and_then(serde_json::Value::as_str)
        .is_some()
    {
        return Ok((RepoProtocol::V2, dist_mirrors));
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
    Ok((
        RepoProtocol::V1(V1Discovery {
            providers_url,
            provider_includes,
        }),
        dist_mirrors,
    ))
}

/// Extract the dist-url mirror templates from a `packages.json` root.
/// Composer's `ComposerRepository::loadRootServerFile`: each `mirrors`
/// entry with a `dist-url` becomes `{url, preferred}`; `git-url` /
/// `hg-url` entries are source mirrors, which bougie (dist-only)
/// skips. Missing or malformed entries are dropped rather than
/// erroring — a bad mirror declaration shouldn't sink the repo.
fn parse_dist_mirrors(root: &serde_json::Value, repo_url: &str) -> Vec<DistMirror> {
    let Some(list) = root.get("mirrors").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|entry| {
            let url = entry.get("dist-url")?.as_str()?;
            // PHP-truthy check, like Composer's `!empty($mirror['preferred'])`:
            // JSON true, or a nonzero number.
            let preferred = match entry.get("preferred") {
                Some(serde_json::Value::Bool(b)) => *b,
                Some(serde_json::Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
                _ => false,
            };
            Some(DistMirror { url: canonicalize_mirror_url(repo_url, url), preferred })
        })
        .collect()
}

/// Composer's `ComposerRepository::canonicalizeUrl` for mirror
/// templates: a URL starting with `/` is host-relative — prepend the
/// repo's `scheme://host`. Anything else (already absolute, or a
/// shape we don't recognize) passes through verbatim.
fn canonicalize_mirror_url(repo_url: &str, url: &str) -> String {
    if !url.starts_with('/') {
        return url.to_owned();
    }
    if let Some((scheme, rest)) = repo_url.split_once("://") {
        let host = rest.split('/').next().unwrap_or("");
        if !host.is_empty() {
            return format!("{scheme}://{host}{url}");
        }
    }
    url.to_owned()
}

/// Which of the two `/p2/` documents to fetch for a package.
///
/// Packagist publishes stable releases in `<name>.json` and any
/// `dev-*` / branch versions in `<name>~dev.json`. Most resolves only
/// need the first; the dev variant matters when a `composer.json`
/// pins `dev-main` (or similar) or when `minimum-stability` allows
/// branch installs.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
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
        req = req.header(auth.header_name(), auth.header_value());
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
        detail: error_chain(&e),
    })?;
    tracing::debug!(
        url = %url,
        status = %resp.status(),
        elapsed_ms = crate::elapsed_ms(start.elapsed()),
        package = %package_name,
        variant = ?variant,
        phase = "fetch",
        "GET",
    );

    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return read_cached(&json_path).map(|md| apply_dist_mirrors(md, repo));
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
        detail: error_chain(&e),
    })?;

    write_atomic(&json_path, &bytes)?;
    if let Some(t) = etag {
        let _ = write_atomic(&etag_path, t.as_bytes());
    }

    PackageMetadata::parse(&bytes)
        .wrap_err_with(|| format!("parsing metadata for {package_name}"))
        .map(|md| apply_dist_mirrors(md, repo))
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
        req = req.header(auth.header_name(), auth.header_value());
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
        detail: error_chain(&e),
    })?;
    tracing::debug!(
        url = %url,
        status = %resp.status(),
        elapsed_ms = crate::elapsed_ms(start.elapsed()),
        package = %package_name,
        variant = ?variant,
        phase = "fetch_optional",
        "GET",
    );

    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return read_cached(&json_path).map(|md| Some(apply_dist_mirrors(md, repo)));
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
        detail: error_chain(&e),
    })?;

    write_atomic(&json_path, &bytes)?;
    if let Some(t) = etag {
        let _ = write_atomic(&etag_path, t.as_bytes());
    }

    PackageMetadata::parse(&bytes)
        .wrap_err_with(|| format!("parsing metadata for {package_name}"))
        .map(|md| Some(apply_dist_mirrors(md, repo)))
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

/// Stamp the repo's discovered dist-mirror templates onto every
/// fetched package version — Composer's
/// `ComposerRepository::createPackages`, which calls `setDistMirrors`
/// on each package it loads. The templates ride the package through
/// resolution and get dumped into the lock (`dist.mirrors`), so
/// install-from-lock can build its mirror-first URL list without
/// re-probing the repo. Applied at every fetch return path (fresh
/// body and disk-cache hit alike — the on-disk cache stores the raw
/// wire bytes, which don't carry the root-level mirrors).
fn apply_dist_mirrors(mut md: PackageMetadata, repo: &Repo) -> PackageMetadata {
    if repo.dist_mirrors.is_empty() {
        return md;
    }
    for versions in md.packages.values_mut() {
        for pkg in versions.iter_mut() {
            if let Some(dist) = pkg.dist.as_mut() {
                dist.mirrors.clone_from(&repo.dist_mirrors);
            }
        }
    }
    md
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
) -> Result<FxHashMap<String, String>> {
    let mut table: FxHashMap<String, String> = FxHashMap::default();
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
        req = req.header(auth.header_name(), auth.header_value());
    }
    let resp = req.send().map_err(|e| BougieError::Network {
        operation: format!("GET {url}"),
        detail: error_chain(&e),
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
        detail: error_chain(&e),
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
    provider_table: &FxHashMap<String, String>,
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
            req = req.header(auth.header_name(), auth.header_value());
        }
        let resp = req.send().map_err(|e| BougieError::Network {
            operation: format!("GET {url}"),
            detail: error_chain(&e),
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
            detail: error_chain(&e),
        })?;
        write_atomic(&cache_path, &body)?;
        body.to_vec()
    };
    parse_v1_per_package(&bytes, package_name).map(|md| Some(apply_dist_mirrors(md, repo)))
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

// ===========================================================================
// Async siblings
// ---------------------------------------------------------------------------
// Used by `update::run_prefetch_fanout`. The blocking versions above
// stay in place for the lazy fallback path in `ResolveProvider::
// load_real_candidates` and for `probe_protocol`/`discover_repos`
// (one-shot, called outside the fan-out). The duplication is ugly
// but the alternative — making the blocking callers go through
// `Runtime::block_on` of the async path — drags a runtime
// requirement into every sync call site.
//
// Filesystem ops (`fs::read`, `fs::write`, etc.) stay sync inside
// these async functions. For the small ETag + JSON files we deal
// with (a few KB), `tokio::fs` adds spawn-blocking overhead without
// material benefit; a blocking syscall on the local cache is cheap
// compared to network RTT.

/// Async sibling of [`fetch_package_metadata_optional`]. Same
/// 404/HTML-fallback behavior, same ETag-conditional-GET shape,
/// same on-disk cache layout — just `client.get(...).send().await`
/// instead of blocking.
pub async fn fetch_package_metadata_optional_async(
    client: &reqwest::Client,
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
        req = req.header(auth.header_name(), auth.header_value());
    }
    if let Ok(etag) = fs::read_to_string(&etag_path) {
        let etag = etag.trim();
        if !etag.is_empty() {
            req = req.header(reqwest::header::IF_NONE_MATCH, etag);
        }
    }

    let start = std::time::Instant::now();
    let resp = req.send().await.map_err(|e| BougieError::Network {
        operation: format!("GET {url}"),
        detail: error_chain(&e),
    })?;
    tracing::debug!(
        url = %url,
        status = %resp.status(),
        elapsed_ms = crate::elapsed_ms(start.elapsed()),
        package = %package_name,
        variant = ?variant,
        phase = "fetch_optional_async",
        "GET",
    );

    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return read_cached(&json_path).map(|md| Some(apply_dist_mirrors(md, repo)));
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

    if response_is_html_async(&resp) {
        return Ok(None);
    }

    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = resp.bytes().await.map_err(|e| BougieError::Network {
        operation: format!("reading body of {url}"),
        detail: error_chain(&e),
    })?;

    write_atomic(&json_path, &bytes)?;
    if let Some(t) = etag {
        let _ = write_atomic(&etag_path, t.as_bytes());
    }

    PackageMetadata::parse(&bytes)
        .wrap_err_with(|| format!("parsing metadata for {package_name}"))
        .map(|md| Some(apply_dist_mirrors(md, repo)))
}

/// True when an async response advertises HTML — same content-type
/// guard as [`response_is_html`], adapted for `reqwest::Response`.
fn response_is_html_async(resp: &reqwest::Response) -> bool {
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

/// Async sibling of [`load_v1_provider_table`]. Same content-
/// addressed disk cache layout, same merge semantics; just
/// `await`s the include fetches sequentially. v1 repos are rare
/// and the includes per repo are few — no need to parallelize the
/// inner loop.
pub async fn load_v1_provider_table_async(
    client: &reqwest::Client,
    paths: &Paths,
    repo: &Repo,
    discovery: &V1Discovery,
) -> Result<FxHashMap<String, String>> {
    let mut table: FxHashMap<String, String> = FxHashMap::default();
    for include in &discovery.provider_includes {
        let body = fetch_v1_include_cached_async(client, paths, repo, include).await?;
        let parsed: serde_json::Value = serde_json::from_slice(&body)
            .wrap_err_with(|| {
                format!(
                    "parsing v1 provider-include from {} (sha256 {})",
                    include.path_template, include.sha256,
                )
            })?;
        let Some(providers) = parsed.get("providers").and_then(serde_json::Value::as_object)
        else {
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

async fn fetch_v1_include_cached_async(
    client: &reqwest::Client,
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
        req = req.header(auth.header_name(), auth.header_value());
    }
    let resp = req.send().await.map_err(|e| BougieError::Network {
        operation: format!("GET {url}"),
        detail: error_chain(&e),
    })?;
    if !resp.status().is_success() {
        return Err(BougieError::Network {
            operation: format!("GET {url}"),
            detail: format!("server returned HTTP {}", resp.status()),
        }
        .into());
    }
    let bytes = resp.bytes().await.map_err(|e| BougieError::Network {
        operation: format!("reading body of {url}"),
        detail: error_chain(&e),
    })?;
    write_atomic(&cache_path, &bytes)?;
    Ok(bytes.to_vec())
}

/// Async sibling of [`fetch_package_metadata_v1_optional`]. Same
/// content-addressed disk cache + 404 semantics.
pub async fn fetch_package_metadata_v1_optional_async(
    client: &reqwest::Client,
    paths: &Paths,
    repo: &Repo,
    discovery: &V1Discovery,
    provider_table: &FxHashMap<String, String>,
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
            req = req.header(auth.header_name(), auth.header_value());
        }
        let resp = req.send().await.map_err(|e| BougieError::Network {
            operation: format!("GET {url}"),
            detail: error_chain(&e),
        })?;
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
        let body = resp.bytes().await.map_err(|e| BougieError::Network {
            operation: format!("reading body of {url}"),
            detail: error_chain(&e),
        })?;
        write_atomic(&cache_path, &body)?;
        body.to_vec()
    };
    parse_v1_per_package(&bytes, package_name).map(|md| Some(apply_dist_mirrors(md, repo)))
}

#[cfg(test)]
mod tests;
