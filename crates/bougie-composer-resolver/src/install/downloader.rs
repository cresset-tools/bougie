//! Parallel Composer dist downloader.
//!
//! Two-phase: download every dist into a persistent content-addressed
//! cache at `$BOUGIE_CACHE/composer-dist/<sha1>.<ext>`, then extract
//! each into its `vendor/<vendor>/<package>/` destination. Splitting
//! the phases means a partial download failure aborts before any
//! extraction starts — `vendor/` is either fully populated by this
//! call (modulo what was already there) or untouched.
//!
//! The cache is keyed by the sha1 hex that Composer publishes as
//! `dist.shasum`. Composer itself keeps the same archives in
//! `~/.composer/cache/files/` so the win here is moving them under
//! bougie's XDG-strict cache root and sharing them across every
//! project bougie installs.

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
    pub sha1: &'a str,
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
}

/// Per-dist outcome reported back to the caller so the install
/// summary can distinguish cache hits ("already had this package's
/// bytes locally") from fresh downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistOutcome {
    CacheHit,
    Downloaded,
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
    let cache_root = paths.cache_composer_dist();
    std::fs::create_dir_all(&cache_root)
        .wrap_err_with(|| format!("creating {}", cache_root.display()))?;

    let outcomes: Vec<DistOutcome> = dists
        .par_iter()
        .map(|d| download_to_cache(client, &cache_root, d, bar))
        .collect::<Result<Vec<_>>>()?;
    dists
        .par_iter()
        .try_for_each(|d| extract_from_cache(&cache_root, d))?;
    Ok(outcomes)
}

/// Download one dist into the content-addressed cache. No-op when the
/// cache already has a verified copy (filename match = sha1 match,
/// because the rename is atomic and only happens after verification).
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
    let spec = bougie_fetch::BlobSpec {
        url: dist.url,
        hash: Hash::sha1(dist.sha1),
        partial_dir: cache_root,
        dest: &cache_path,
        // `fetch_file` doesn't extract, so `strip_prefix` / `archive`
        // are inert here. We keep them populated for symmetry with
        // `fetch_blob` callers — and because BlobSpec literals require
        // every field.
        strip_prefix: "",
        archive: dist.archive,
    };
    bougie_fetch::fetch_file(client, &spec, bar).wrap_err_with(|| {
        format!("downloading dist for {}", dist.package_name)
    })?;
    Ok(DistOutcome::Downloaded)
}

/// Extract one cached dist archive into its `vendor_dest`. The
/// destination is wiped beforehand so the call is idempotent — a
/// previous half-done install does not poison the new tree.
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
            let strip = match dist.strip_prefix {
                Some(s) => s,
                None => {
                    detected = bougie_fetch::detect_zip_top_level(&cache_path)
                        .wrap_err_with(|| {
                            format!(
                                "detecting top-level dir in dist for {} ({})",
                                dist.package_name,
                                cache_path.display(),
                            )
                        })?;
                    detected.as_str()
                }
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
        ArchiveKind::TarZst => {
            return Err(eyre::eyre!(
                "TarZst is not a Composer dist format; package {} has the wrong archive kind",
                dist.package_name
            ));
        }
    }
    Ok(())
}

/// `$BOUGIE_CACHE/composer-dist/<sha1>.<ext>`. The extension is for
/// human-readable cache listings only; lookup is keyed on the hash.
fn cache_path_for(cache_root: &Path, dist: &DistRequest<'_>) -> PathBuf {
    let ext = match dist.archive {
        ArchiveKind::Zip => "zip",
        ArchiveKind::TarZst => "tar.zst",
    };
    cache_root.join(format!("{}.{ext}", dist.sha1))
}

#[cfg(test)]
mod tests;
