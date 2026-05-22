//! `install_from_lock` — the orchestrator behind `bougie composer
//! install`.
//!
//! Reads `composer.json` + `composer.lock`, verifies content-hash,
//! runs preflight (rejecting anything Phase A doesn't yet handle —
//! plugins, path/tar/vcs dists, post-install scripts), builds
//! [`DistRequest`]s, calls [`fetch_and_extract_dists`] to populate
//! `vendor/`, then hands off to `bougie_autoloader::dump_autoload` to
//! emit `vendor/autoload.php` + `vendor/composer/installed.{json,php}`.
//!
//! Preflight failures are aggregated into a single error so the user
//! sees every blocker in one pass rather than fix-one-hit-next.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bougie_autoloader::{dump_autoload, DumpRequest};
use bougie_composer::lockfile::{self, Lock, LockPackage};
use bougie_fetch::{ArchiveKind, DownloadBar};
use bougie_paths::Paths;
use eyre::{eyre, Context, Result};
use serde_json::Value;

use crate::metadata::AuthCredentials;
use crate::update::{read_auth_from_composer_json, read_auth_json};

use super::downloader::{fetch_and_extract_dists, DistOutcome, DistRequest};

/// Caller-supplied install options. Mirrors the subset of Composer's
/// `install` flags we honor in Phase A.
#[derive(Debug, Clone, Copy, Default)]
pub struct InstallOptions {
    /// Skip packages in `composer.lock`'s `packages-dev` AND pass
    /// `no_dev=true` to the autoloader so dev autoload entries
    /// don't reach `vendor/autoload.php`.
    pub no_dev: bool,
}

/// What happened. Returned to the CLI shim for `--format json-v1`
/// emission and rendered as a one-line text summary.
#[derive(Debug, Clone)]
pub struct InstallSummary {
    pub project_root: PathBuf,
    pub packages_installed: u32,
    pub packages_already_present: u32,
    pub no_dev: bool,
}

/// Apply `composer.lock` to `project_root`. See module docs for the
/// flow.
pub fn install_from_lock(
    paths: &Paths,
    project_root: &Path,
    opts: InstallOptions,
) -> Result<InstallSummary> {
    let composer_json_path = project_root.join("composer.json");
    let composer_lock_path = project_root.join("composer.lock");
    let composer_json_bytes = std::fs::read(&composer_json_path).wrap_err_with(|| {
        format!(
            "{} not found — not a Composer project",
            composer_json_path.display()
        )
    })?;
    let lock = if composer_lock_path.exists() {
        Lock::read(&composer_lock_path)?
    } else {
        return Err(eyre!(
            "{} not found — run `bougie run -- composer update` first to generate it",
            composer_lock_path.display()
        ));
    };

    verify_content_hash(&composer_json_bytes, &lock)?;
    preflight(&composer_json_bytes, &lock, opts.no_dev)?;

    // Assemble per-host auth the same way the resolver does for
    // metadata requests: composer.json's `config.http-basic` /
    // `config.bearer` first, then project-level `auth.json` (which
    // wins on conflicts — matches Composer's precedence). Dist URLs
    // sitting behind the same auth as the metadata (Magento's
    // `/archives/...`, private satis, GitLab CI Composer ZIPs) need
    // the header; public-CDN dists from Packagist do not.
    let composer_json_value: Value = serde_json::from_slice(&composer_json_bytes)
        .map_err(|e| eyre!("parsing composer.json: {e}"))?;
    let mut auth: HashMap<String, AuthCredentials> =
        read_auth_from_composer_json(&composer_json_value).map_err(|e| eyre!(e))?;
    auth.extend(read_auth_json(project_root).map_err(|e| eyre!(e))?);

    // Gather the packages we'll actually install. `path` dists are
    // skipped silently here: preflight already rejected them when
    // `opts` would make them install-time relevant, but a stray
    // path-dist entry in a project that the user is comfortable
    // with shouldn't block install — the autoloader treats them by
    // reading the lock anyway.
    let install_set: Vec<&LockPackage> = if opts.no_dev {
        lock.packages.iter().filter(|p| !p.is_path_dist()).collect()
    } else {
        lock.all_packages().filter(|p| !p.is_path_dist()).collect()
    };

    // Each DistRequest borrows from the LockPackage; build the
    // ancillary owned data (vendor dest paths, archive enums) in a
    // sibling vec so the borrows in `DistRequest` line up.
    let vendor_dirs: Vec<PathBuf> = install_set
        .iter()
        .map(|p| project_root.join("vendor").join(&p.name))
        .collect();
    // Pre-render each dist's `Authorization` header (when its host
    // matches the auth map). String storage lives in a sibling vec so
    // `DistRequest` can carry a borrowed `&str` — no per-request
    // clones, no lifetime gymnastics inside `par_iter`.
    let auth_headers: Vec<Option<String>> = install_set
        .iter()
        .map(|p| {
            let dist = p.dist.as_ref().unwrap();
            host_from_url(&dist.url)
                .and_then(|host| auth.get(host))
                .map(|creds| creds.header_value())
        })
        .collect();
    let dists: Vec<DistRequest<'_>> = install_set
        .iter()
        .zip(vendor_dirs.iter())
        .zip(auth_headers.iter())
        .map(|((p, dest), auth_header)| {
            // unwraps: preflight guarantees every survivor has
            // `dist` with kind=="zip" and `shasum` Some.
            let dist = p.dist.as_ref().unwrap();
            DistRequest {
                package_name: &p.name,
                url: &dist.url,
                sha1: dist.shasum.as_deref().unwrap(),
                archive: ArchiveKind::Zip,
                strip_prefix: None,
                vendor_dest: dest,
                auth_header: auth_header.as_deref(),
            }
        })
        .collect();

    let client = reqwest::blocking::Client::new();
    let bar = DownloadBar::new("downloading");
    let outcomes = fetch_and_extract_dists(&client, paths, &dists, &bar)?;
    bar.finish();

    dump_autoload(&DumpRequest {
        project_root,
        optimize: false,
        classmap_authoritative: false,
        no_dev: opts.no_dev,
        apcu_autoloader: false,
        apcu_prefix: None,
        autoloader_suffix: None,
    })
    .map_err(|e| eyre!("autoload dump failed: {e}"))?;

    let packages_installed = outcomes
        .iter()
        .filter(|o| **o == DistOutcome::Downloaded)
        .count() as u32;
    let packages_already_present = outcomes
        .iter()
        .filter(|o| **o == DistOutcome::CacheHit)
        .count() as u32;
    Ok(InstallSummary {
        project_root: project_root.to_path_buf(),
        packages_installed,
        packages_already_present,
        no_dev: opts.no_dev,
    })
}

/// Extract the host portion of a URL — the bit between `://` and
/// the next `/`, with any `:port` suffix stripped. Returns `None`
/// for URLs without a parseable host (e.g. file URIs in tests).
/// Used to key per-host auth lookup the same way Composer does;
/// path differences inside the host don't matter (everything under
/// `repo.magento.com` shares the same credentials whether the URL
/// targets `/p/...` for metadata or `/archives/...` for a dist).
fn host_from_url(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://")?.1;
    let host_and_port = after_scheme.split('/').next()?;
    let host = host_and_port.split(':').next()?;
    if host.is_empty() { None } else { Some(host) }
}

/// Verify the lock's `content-hash` field against the current
/// `composer.json` bytes, using the same algorithm Composer itself
/// runs (delegated to `bougie_composer::lockfile::content_hash`).
fn verify_content_hash(composer_json_bytes: &[u8], lock: &Lock) -> Result<()> {
    let Some(expected) = &lock.content_hash else {
        // Pre-1.10 lockfiles don't carry a content-hash. Composer
        // tolerates them; we do too rather than refuse to install a
        // perfectly working historical project.
        return Ok(());
    };
    let actual = lockfile::content_hash(composer_json_bytes)?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(eyre!(
            "composer.lock is out of sync with composer.json (content-hash {} → {}). \
             Run `bougie run -- composer update` to regenerate the lock.",
            expected,
            actual,
        ));
    }
    Ok(())
}

/// Reject anything Phase A doesn't yet handle. Aggregates every
/// failure into a single error so the user sees the full picture.
fn preflight(composer_json_bytes: &[u8], lock: &Lock, no_dev: bool) -> Result<()> {
    let mut reasons: Vec<String> = Vec::new();

    // composer.json scripts → fallback, we don't run them.
    if let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(composer_json_bytes) {
        if obj
            .get("scripts")
            .and_then(Value::as_object)
            .is_some_and(|s| !s.is_empty())
        {
            reasons.push(
                "composer.json declares `scripts` (post-install / post-autoload-dump etc.); \
                 bougie does not yet run them. Use `bougie run -- composer install`."
                    .into(),
            );
        }
    }

    let packages: Vec<&LockPackage> = if no_dev {
        lock.packages.iter().collect()
    } else {
        lock.all_packages().collect()
    };

    for p in packages {
        // path dists materialize via symlink-or-copy — Composer's
        // own logic outside the dist-archive flow. We skip these
        // during install (see `install_from_lock`) but a project
        // that has *only* path dists works fine; no rejection here.
        if p.is_path_dist() {
            continue;
        }
        if p.is_composer_plugin() {
            reasons.push(format!(
                "package `{}` is a Composer plugin (type `composer-plugin`); \
                 bougie does not yet run plugin install-time hooks. \
                 Use `bougie run -- composer install`.",
                p.name,
            ));
            continue;
        }
        let Some(dist) = &p.dist else {
            reasons.push(format!(
                "package `{}` has no `dist` block (source-only install); \
                 bougie does not yet clone VCS sources. \
                 Use `bougie run -- composer install`.",
                p.name,
            ));
            continue;
        };
        if dist.kind != "zip" {
            reasons.push(format!(
                "package `{}` uses dist type `{}`; bougie's installer \
                 currently supports only zip dists. \
                 Use `bougie run -- composer install`.",
                p.name, dist.kind,
            ));
            continue;
        }
        if dist.shasum.is_none() {
            reasons.push(format!(
                "package `{}` has no `dist.shasum`; bougie requires \
                 verifiable archives. \
                 Use `bougie run -- composer install`.",
                p.name,
            ));
        }
    }

    if reasons.is_empty() {
        Ok(())
    } else {
        let bullets = reasons
            .iter()
            .map(|r| format!("  - {r}"))
            .collect::<Vec<_>>()
            .join("\n");
        Err(eyre!(
            "this lockfile requires features bougie's install does not yet handle:\n{bullets}",
        ))
    }
}

#[cfg(test)]
mod tests;
