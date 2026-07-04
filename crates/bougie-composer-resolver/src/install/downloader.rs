//! Parallel Composer dist downloader.
//!
//! Two-phase: download every dist into a persistent cache at
//! `$BOUGIE_CACHE/composer-dist/<key>.<ext>`, then extract each into
//! its `vendor/<vendor>/<package>/` destination. Splitting the phases
//! means a partial download failure aborts before any extraction starts
//! — `vendor/` is either fully populated by this call (modulo what was
//! already there) or untouched.
//!
//! `<key>` is the sha1 hex Composer publishes as `dist.shasum` when
//! present, falling back to the dist's git `reference` when shasum is
//! empty (the normal case for GitHub/GitLab zipball dists — see
//! Composer's `GitHubDriver::getDist()`, which emits `'shasum' => ''`).
//! With a real shasum the cache is genuinely content-addressed; with a
//! reference fallback it's content-*coordinate*-addressed — the git ref
//! locks the upstream tree, so the bytes are stable in practice even if
//! GitHub doesn't guarantee them per-request.
//!
//! Composer itself keeps the same archives in `~/.composer/cache/files/`
//! under a `<vendor>/<name>/<ref>.zip` layout; we differ only in the key
//! shape, not the cache semantics.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use bougie_fetch::{ArchiveKind, DownloadBar, Hash};
use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use rayon::prelude::*;

/// One package to materialize into `vendor/`. Built by the eventual
/// lockfile reader from a `composer.lock` `packages[]` entry; in tests
/// we construct these by hand against fixture HTTP servers.
#[derive(Debug, Clone, Copy)]
pub struct DistRequest<'a> {
    /// Canonical Composer package name (`vendor/package`). Used purely
    /// for the bar label and error messages — the cache key is the
    /// hash, not the name.
    pub package_name: &'a str,
    /// `dist.url` straight from the lockfile.
    pub url: &'a str,
    /// `dist.shasum` straight from the lockfile (sha1 hex, lower-case).
    /// Empty string when the upstream registry doesn't publish one —
    /// the common case for GitHub/GitLab/Bitbucket zipball dists,
    /// which is most of public Packagist. When empty, the downloader
    /// skips post-download verification (matching Composer's
    /// `FileDownloader.php:212` behavior) and falls back to
    /// [`reference`](Self::reference) as the cache key.
    pub sha1: &'a str,
    /// `dist.reference` from the lockfile — the upstream git ref
    /// (full sha for git). Used as the cache key when `sha1` is empty.
    /// Populated for every VCS-driver dist; only `path` dists lack a
    /// reference, and those are filtered out before this struct is
    /// built.
    pub reference: &'a str,
    /// Archive format. Composer publishes mostly zip; tar.gz appears
    /// for some packages and is not yet supported (the variant will
    /// error at extraction time — accepted limitation for the
    /// install-from-lock MVP per `RESOLVER_PLAN.md` Phase A).
    pub archive: ArchiveKind,
    /// Top-level directory inside the archive to strip — e.g.
    /// `monolog-monolog-1234567`. `None` (the default for callers
    /// driven by `composer.lock`) means auto-detect from the cached
    /// archive's central directory via
    /// [`bougie_fetch::detect_zip_top_level`]; pass `Some` only when
    /// the caller already knows the wrapper name (e.g. tests built
    /// around a fixture zip with a known layout).
    pub strip_prefix: Option<&'a str>,
    /// Where the extracted tree should live. Typically
    /// `<project>/vendor/<vendor>/<package>/`. The directory is
    /// created (and any existing contents replaced) by the extractor.
    pub vendor_dest: &'a Path,
    /// Pre-rendered `Authorization` header value (e.g. `Basic <b64>`
    /// or `Bearer <token>`) attached to the GET that fetches this
    /// dist. `None` for public dists (Packagist's CDN); set by the
    /// orchestrator when the dist URL's host matches an entry in
    /// composer.json's `config.http-basic` / `config.bearer` or
    /// project-level `auth.json`. Matches Composer's behavior of
    /// sending the same per-host creds to dist URLs as it sends to
    /// the corresponding metadata URLs.
    pub auth_header: Option<&'a str>,
    pub auth_header_name: Option<&'a str>,
    /// Project root, used to resolve non-http dist URLs (Composer
    /// `type: artifact` repositories serialize the artifact zip's
    /// path straight into `dist.url` as a project-relative string).
    /// Tests that only exercise HTTP dists can pass any path.
    pub project_root: &'a Path,
    /// Fallback download locations, tried in order when the GET
    /// against [`url`](Self::url) fails — Composer's dist-mirror
    /// semantics (`Package::getDistUrls` + `FileDownloader`'s
    /// try-next-URL loop). The orchestrator builds the full candidate
    /// list from the lock's `dist.mirrors` (preferred mirror first),
    /// puts the first candidate in `url` and the rest here. Empty for
    /// the overwhelming majority of dists — public Packagist declares
    /// no mirrors.
    pub fallbacks: &'a [DistCandidate],
}

/// One alternative download location for a dist: a fully substituted
/// mirror URL plus its pre-rendered per-host auth header. Owned
/// strings (unlike the borrowed fields on [`DistRequest`]) because the
/// URLs are produced by placeholder substitution at request-build
/// time and have no longer-lived home to borrow from.
#[derive(Debug, Clone)]
pub struct DistCandidate {
    pub url: String,
    pub auth_header: Option<String>,
    pub auth_header_name: Option<&'static str>,
}

/// Per-dist outcome reported back to the caller so the install
/// summary can distinguish cache hits ("already had this package's
/// bytes locally") from fresh downloads. `Downloaded` carries the
/// archive size on disk — the telemetry `download_bytes` counter and
/// nothing else; it is not a transfer-accurate byte count (compression
/// and resume both make that a different number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistOutcome {
    CacheHit,
    Downloaded { bytes: u64 },
}

/// Download every dist in `dists` in parallel into the bougie cache,
/// then extract each into its `vendor_dest`. Returns once every
/// extraction is complete; on any failure, the remaining downloads
/// still finish (rayon's `try_for_each` aborts the *result*, not the
/// work in flight) but extraction does not start.
///
/// Outcomes are returned in the same order as `dists`.
pub fn fetch_and_extract_dists(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    dists: &[DistRequest<'_>],
    bar: &DownloadBar,
) -> Result<Vec<DistOutcome>> {
    fetch_and_extract_dists_with_progress(client, paths, dists, bar, |_, _| {}, |_| {})
}

/// Like [`fetch_and_extract_dists`], but invokes `on_dist_done` once per
/// dist as its download phase resolves (cache hit or fresh download) and
/// `on_extract_done` once per dist as its extraction completes. Used by
/// the install orchestrator to drive a per-package progress bar: the
/// lockfile doesn't carry dist sizes, so a bytes-based bar would be
/// misleading — we tick once per finished package instead.
#[tracing::instrument(skip_all, fields(dists = dists.len()))]
pub fn fetch_and_extract_dists_with_progress<D, X>(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    dists: &[DistRequest<'_>],
    bar: &DownloadBar,
    on_dist_done: D,
    on_extract_done: X,
) -> Result<Vec<DistOutcome>>
where
    D: Fn(&str, DistOutcome) + Sync,
    X: Fn(&str) + Sync,
{
    let cache_root = paths.cache_composer_dist();
    std::fs::create_dir_all(&cache_root)
        .wrap_err_with(|| format!("creating {}", cache_root.display()))?;

    let outcomes: Vec<DistOutcome> = dists
        .par_iter()
        .map(|d| {
            let outcome = download_to_cache(client, &cache_root, d, bar)?;
            on_dist_done(d.package_name, outcome);
            Ok(outcome)
        })
        .collect::<Result<Vec<_>>>()?;
    dists.par_iter().try_for_each(|d| {
        extract_from_cache(&cache_root, d)?;
        on_extract_done(d.package_name);
        Ok::<_, eyre::Report>(())
    })?;
    Ok(outcomes)
}

/// Download one dist into the content-addressed cache. No-op when the
/// cache already has a verified copy (filename match = sha1 match,
/// because the rename is atomic and only happens after verification).
#[tracing::instrument(skip_all, fields(package = dist.package_name))]
fn download_to_cache(
    client: &reqwest::blocking::Client,
    cache_root: &Path,
    dist: &DistRequest<'_>,
    bar: &DownloadBar,
) -> Result<DistOutcome> {
    let cache_path = cache_path_for(cache_root, dist);
    if cache_path.exists() {
        return Ok(DistOutcome::CacheHit);
    }
    bar.set_current(dist.package_name);
    if !is_http_url(dist.url) {
        return copy_local_dist(dist, &cache_path);
    }
    // Composer's FileDownloader loop: try each candidate URL in order,
    // warn-and-continue on failure, surface the last error when every
    // candidate is exhausted. `fallbacks` is empty for repos without
    // dist mirrors, so the common path is a single attempt.
    let candidates = std::iter::once((dist.url, dist.auth_header, dist.auth_header_name)).chain(
        dist.fallbacks
            .iter()
            .map(|c| (c.url.as_str(), c.auth_header.as_deref(), c.auth_header_name)),
    );
    let total = 1 + dist.fallbacks.len();
    let mut last_err: Option<eyre::Report> = None;
    for (idx, (url, auth_header, auth_header_name)) in candidates.enumerate() {
        let url = rewrite_github_dist_url(url);
        let spec = bougie_fetch::BlobSpec {
            url: &url,
            hash: Hash::sha1(dist.sha1),
            partial_dir: cache_root,
            dest: &cache_path,
            strip_prefix: "",
            archive: dist.archive,
            auth_header,
            auth_header_name,
        };
        match bougie_fetch::fetch_file(client, &spec, bar) {
            Ok(_) => {
                return Ok(DistOutcome::Downloaded { bytes: file_size(&cache_path) });
            }
            Err(err) => {
                if idx + 1 < total {
                    tracing::warn!(
                        package = dist.package_name,
                        url = %url,
                        error = %err,
                        "dist download failed; trying the next mirror",
                    );
                }
                last_err = Some(err.wrap_err(format!(
                    "downloading dist for {} from {:?}",
                    dist.package_name, url,
                )));
            }
        }
    }
    // The candidate iterator always yields at least the primary URL,
    // so reaching this point means the loop ran and stored an error.
    let err = last_err.expect("download loop ran at least once");
    if total > 1 {
        Err(err.wrap_err(format!(
            "downloading dist for {}: all {} candidate URLs failed",
            dist.package_name, total,
        )))
    } else {
        Err(err)
    }
}

fn is_http_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Materialize a Composer `type: artifact` dist into the cache by
/// copying the local zip. Composer serializes the artifact's path as
/// `dist.url` relative to the project root (e.g.
/// `artifacts/vendor-pkg-1.2.3.zip`) — resolve it against `project_root`,
/// verify the sha1 if the lockfile carries one, then copy into the
/// content-addressed cache so the extraction phase stays
/// transport-agnostic. Absolute paths and `file://` URLs are accepted as
/// they appear; relative paths are joined onto `project_root`.
fn copy_local_dist(dist: &DistRequest<'_>, cache_path: &Path) -> Result<DistOutcome> {
    let raw = dist.url.strip_prefix("file://").unwrap_or(dist.url);
    let candidate = Path::new(raw);
    let src = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        dist.project_root.join(candidate)
    };
    if !src.exists() {
        return Err(eyre::eyre!(
            "dist for {} points to local file {} which does not exist \
             (Composer `type: artifact` repository file missing?)",
            dist.package_name,
            src.display(),
        ));
    }
    if !dist.sha1.is_empty() {
        verify_local_sha1(&src, dist.sha1).wrap_err_with(|| {
            format!("verifying local dist for {}", dist.package_name)
        })?;
    }
    std::fs::copy(&src, cache_path).wrap_err_with(|| {
        format!(
            "copying local dist for {} ({} → {})",
            dist.package_name,
            src.display(),
            cache_path.display(),
        )
    })?;
    Ok(DistOutcome::Downloaded { bytes: file_size(cache_path) })
}

/// Best-effort on-disk size for the telemetry byte counter.
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

fn verify_local_sha1(path: &Path, expected_hex: &str) -> Result<()> {
    use sha1::Digest as _;
    let mut file = std::fs::File::open(path)
        .wrap_err_with(|| format!("opening {}", path.display()))?;
    let mut hasher = sha1::Sha1::new();
    std::io::copy(&mut file, &mut hasher)
        .wrap_err_with(|| format!("hashing {}", path.display()))?;
    let actual = hasher.finalize();
    let mut actual_hex = String::with_capacity(40);
    for b in actual {
        use std::fmt::Write as _;
        let _ = write!(actual_hex, "{b:02x}");
    }
    if !actual_hex.eq_ignore_ascii_case(expected_hex) {
        return Err(eyre::eyre!(
            "sha1 mismatch on {}: expected {}, got {}",
            path.display(),
            expected_hex,
            actual_hex,
        ));
    }
    Ok(())
}

/// Rewrite `api.github.com` zipball URLs to `codeload.github.com`
/// direct downloads. The API endpoint returns a 302 redirect to
/// codeload anyway, but the redirect itself consumes one GitHub REST
/// API rate-limit point (60/hr unauthenticated, 5 000/hr with a
/// token). Going directly to codeload skips the redirect and avoids
/// the rate-limit hit entirely — significant for projects with dozens
/// of GitHub-hosted dependencies.
///
/// The `legacy.zip` codeload path produces archives byte-identical to
/// what the API redirect targets (same etag, same `{owner}-{repo}-
/// {short_sha}/` wrapper directory).
///
/// Non-GitHub URLs pass through unchanged.
fn rewrite_github_dist_url(url: &str) -> Cow<'_, str> {
    const PREFIX: &str = "https://api.github.com/repos/";
    const ZIPBALL: &str = "/zipball/";

    let Some(rest) = url.strip_prefix(PREFIX) else {
        return Cow::Borrowed(url);
    };
    let Some(idx) = rest.find(ZIPBALL) else {
        return Cow::Borrowed(url);
    };
    let owner_repo = &rest[..idx];
    let reference = &rest[idx + ZIPBALL.len()..];
    if owner_repo.is_empty() || reference.is_empty() {
        return Cow::Borrowed(url);
    }
    Cow::Owned(format!(
        "https://codeload.github.com/{owner_repo}/legacy.zip/{reference}"
    ))
}

/// Extract one cached dist archive into its `vendor_dest`. The
/// destination is wiped beforehand so the call is idempotent — a
/// previous half-done install does not poison the new tree.
#[tracing::instrument(skip_all, fields(package = dist.package_name))]
fn extract_from_cache(cache_root: &Path, dist: &DistRequest<'_>) -> Result<()> {
    let cache_path = cache_path_for(cache_root, dist);
    if let Some(parent) = dist.vendor_dest.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    let _ = std::fs::remove_dir_all(dist.vendor_dest);
    std::fs::create_dir_all(dist.vendor_dest)
        .wrap_err_with(|| format!("creating {}", dist.vendor_dest.display()))?;
    match dist.archive {
        ArchiveKind::Zip => {
            let detected: String;
            let strip = if let Some(s) = dist.strip_prefix { s } else {
                detected = bougie_fetch::detect_zip_top_level(&cache_path)
                    .wrap_err_with(|| {
                        format!(
                            "detecting top-level dir in dist for {} ({})",
                            dist.package_name,
                            cache_path.display(),
                        )
                    })?;
                detected.as_str()
            };
            bougie_fetch::extract_zip(&cache_path, dist.vendor_dest, strip)
                .wrap_err_with(|| {
                    format!(
                        "extracting dist for {} ({} → {})",
                        dist.package_name,
                        cache_path.display(),
                        dist.vendor_dest.display(),
                    )
                })?;
        }
        ArchiveKind::TarZst | ArchiveKind::TarGz => {
            return Err(eyre::eyre!(
                "{:?} is not a Composer dist format; package {} has the wrong archive kind",
                dist.archive,
                dist.package_name
            ));
        }
    }
    Ok(())
}

/// `$BOUGIE_CACHE/composer-dist/<key>.<ext>`. The extension is for
/// human-readable cache listings only; lookup is keyed on the hash
/// (or the git reference when the upstream didn't publish a hash).
fn cache_path_for(cache_root: &Path, dist: &DistRequest<'_>) -> PathBuf {
    let ext = match dist.archive {
        ArchiveKind::Zip => "zip",
        ArchiveKind::TarZst => "tar.zst",
        ArchiveKind::TarGz => "tar.gz",
    };
    let key = if !dist.sha1.is_empty() {
        // A sha1 shasum is already a safe hex string and content-addresses
        // the archive, so use it verbatim.
        dist.sha1.to_string()
    } else if !dist.reference.is_empty() {
        // The git reference can contain `/` (branch names) or even `..`,
        // which would land in an uncreated subdir (ENOENT) or escape the
        // cache root. Hash it into a flat, traversal-safe token.
        use sha1::Digest as _;
        let digest = sha1::Sha1::digest(dist.reference.as_bytes());
        format!("ref-{digest:x}")
    } else {
        // Neither a content hash nor an upstream reference — the shape a
        // Composer `type: package` repository entry takes (just `dist.url`
        // + `dist.type`). Hashing the empty reference here would collapse
        // *every* such dist onto `ref-<sha1("")>`, so the first package's
        // archive gets silently reused for all the others. Fold in the
        // package name and URL instead, mirroring Composer's `getCacheKey`
        // (which appends `md5($url)` under the package name when a dist
        // has no reference) so distinct packages never collide.
        use sha1::Digest as _;
        let mut hasher = sha1::Sha1::new();
        hasher.update(dist.package_name.as_bytes());
        hasher.update([0]);
        hasher.update(dist.url.as_bytes());
        let digest = hasher.finalize();
        format!("url-{digest:x}")
    };
    cache_root.join(format!("{key}.{ext}"))
}

#[cfg(test)]
mod tests;
