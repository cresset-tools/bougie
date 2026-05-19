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
    /// `monolog-monolog-1234567`. Packagist's zip dists wrap every
    /// entry in a single directory whose name is
    /// `<vendor>-<package>-<short_sha>`; the caller (the eventual lock
    /// reader) computes it from the lock entry, since the lock doesn't
    /// store it directly.
    pub strip_prefix: &'a str,
    /// Where the extracted tree should live. Typically
    /// `<project>/vendor/<vendor>/<package>/`. The directory is
    /// created (and any existing contents replaced) by the extractor.
    pub vendor_dest: &'a Path,
}

/// Download every dist in `dists` in parallel into the bougie cache,
/// then extract each into its `vendor_dest`. Returns once every
/// extraction is complete; on any failure, the remaining downloads
/// still finish (rayon's `try_for_each` aborts the *result*, not the
/// work in flight) but extraction does not start.
pub fn fetch_and_extract_dists(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    dists: &[DistRequest<'_>],
    bar: &DownloadBar,
) -> Result<()> {
    let cache_root = paths.cache_composer_dist();
    std::fs::create_dir_all(&cache_root)
        .wrap_err_with(|| format!("creating {}", cache_root.display()))?;

    dists
        .par_iter()
        .try_for_each(|d| download_to_cache(client, &cache_root, d, bar))?;
    dists
        .par_iter()
        .try_for_each(|d| extract_from_cache(&cache_root, d))?;
    Ok(())
}

/// Download one dist into the content-addressed cache. No-op when the
/// cache already has a verified copy (filename match = sha1 match,
/// because the rename is atomic and only happens after verification).
fn download_to_cache(
    client: &reqwest::blocking::Client,
    cache_root: &Path,
    dist: &DistRequest<'_>,
    bar: &DownloadBar,
) -> Result<()> {
    let cache_path = cache_path_for(cache_root, dist);
    if cache_path.exists() {
        return Ok(());
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
    Ok(())
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
            bougie_fetch::extract_zip(&cache_path, dist.vendor_dest, dist.strip_prefix)
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
