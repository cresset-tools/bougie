//! `install_from_lock` — the orchestrator behind `bougie composer
//! install`.
//!
//! Reads `composer.json` + `composer.lock`, verifies content-hash,
//! runs preflight (rejecting source-only and non-zip dists which bougie
//! cannot install at all; surfacing plugins / post-install scripts as
//! warnings since bougie installs the package zips but never runs their
//! PHP), builds [`DistRequest`]s, calls [`fetch_and_extract_dists`] to
//! populate `vendor/`, then hands off to `bougie_autoloader::dump_autoload`
//! to emit `vendor/autoload.php` + `vendor/composer/installed.{json,php}`.
//!
//! Preflight failures are aggregated into a single error so the user
//! sees every blocker in one pass rather than fix-one-hit-next.
//! Preflight warnings are returned alongside on success and surfaced
//! to the user via [`InstallSummary::warnings`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bougie_autoloader::{dump_autoload, DumpRequest};
use bougie_composer::lockfile::{self, Lock, LockPackage};
use bougie_fetch::{ArchiveKind, DownloadBar};
use bougie_paths::Paths;
use eyre::{eyre, Context, Result};
use serde_json::Value;

use crate::metadata::AuthCredentials;
use crate::update::read_all_auth;

use super::downloader::{fetch_and_extract_dists_with_progress, DistOutcome, DistRequest};

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
    /// Composer-plugin packages we skipped over (their zip was not
    /// extracted because bougie won't run plugin install-time PHP and
    /// the extracted tree would be inert).
    pub packages_skipped_plugin: u32,
    pub bins_installed: u32,
    pub no_dev: bool,
    /// Soft preflight findings — one entry per Composer plugin and one
    /// entry for a non-empty `scripts` section. The CLI prints these
    /// as `warning: …` lines to stderr.
    pub warnings: Vec<String>,
}

/// Apply `composer.lock` to `project_root`. See module docs for the
/// flow.
///
/// # Panics
///
/// Panics on internal preflight invariant violations — the inner
/// unwrap on `p.dist` relies on `preflight` having already rejected
/// source-only packages. `dist.shasum` may be missing/empty (normal
/// for GitHub-zipball dists); the downloader treats empty as
/// skip-verify and keys its cache off `dist.reference` instead. If
/// you changed the preflight rules and forgot to update this
/// consumer, you'll hit the unwrap; that's the failure mode the
/// comment at the unwrap is guarding against.
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
    let mut warnings = preflight(&composer_json_bytes, &lock, opts.no_dev)?;

    // Assemble per-host auth from every source bougie understands —
    // composer.json `config`, global `$COMPOSER_HOME/auth.json`,
    // project-level `auth.json`, and the `COMPOSER_AUTH` env var.
    // See `read_all_auth` for the precedence rationale. Dist URLs
    // sitting behind the same auth as the metadata (Magento's
    // `/archives/...`, private satis, GitLab CI Composer ZIPs) need
    // the header; public-CDN dists from Packagist do not.
    let composer_json_value: Value = serde_json::from_slice(&composer_json_bytes)
        .map_err(|e| eyre!("parsing composer.json: {e}"))?;
    let auth: HashMap<String, AuthCredentials> =
        read_all_auth(&composer_json_value, project_root).map_err(|e| eyre!(e))?;

    // Gather the packages we'll actually install. Two filters:
    //   - `path` dists: skipped silently here. Preflight already
    //     rejected them when `opts` would make them install-time
    //     relevant, but a stray path-dist entry in a project that
    //     the user is comfortable with shouldn't block install — the
    //     autoloader treats them by reading the lock anyway.
    //   - composer-plugin packages: preflight warned about them.
    //     We don't extract their zip because bougie won't run the
    //     plugin's install-time hook and the extracted tree would be
    //     inert (autoload entries pointing at code nothing loads).
    //   - metapackages: no `dist` and no code by definition; they
    //     exist purely as require-graph nodes.
    let candidates: Vec<&LockPackage> = if opts.no_dev {
        lock.packages.iter().collect()
    } else {
        lock.all_packages().collect()
    };
    let packages_skipped_plugin = u32::try_from(
        candidates.iter().filter(|p| p.is_composer_plugin()).count(),
    )
    .unwrap_or(u32::MAX);
    let install_set: Vec<&LockPackage> = candidates
        .iter()
        .copied()
        .filter(|p| !p.is_path_dist() && !p.is_composer_plugin() && !p.is_metapackage())
        .collect();

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
    let auth_entries: Vec<Option<(String, &'static str)>> = install_set
        .iter()
        .map(|p| {
            let dist = p.dist.as_ref().unwrap();
            host_from_url(&dist.url)
                .and_then(|host| auth.get(host))
                .map(|creds| (creds.header_value(), creds.header_name()))
        })
        .collect();
    let dists: Vec<DistRequest<'_>> = install_set
        .iter()
        .zip(vendor_dirs.iter())
        .zip(auth_entries.iter())
        .map(|((p, dest), auth_entry)| {
            let dist = p.dist.as_ref().unwrap();
            DistRequest {
                package_name: &p.name,
                url: &dist.url,
                sha1: dist.shasum.as_deref().unwrap_or(""),
                reference: dist.reference.as_deref().unwrap_or(""),
                archive: ArchiveKind::Zip,
                strip_prefix: None,
                vendor_dest: dest,
                auth_header: auth_entry.as_ref().map(|(v, _)| v.as_str()),
                auth_header_name: auth_entry.as_ref().map(|(_, n)| *n),
            }
        })
        .collect();

    // Use the bougie shared client so dist fetches carry the same
    // `User-Agent` and timeout policy as metadata fetches. Before this
    // the install path built a bare `reqwest::blocking::Client::new()`
    // with no UA and no per-request budget — that got `403`s from
    // Composer-protocol servers (repo.magento.com etc.) which gate on
    // a `Composer/…` UA, and had no upper bound on a runaway dist
    // download.
    let client = bougie_fetch::default_client()?;
    // Composer lockfiles don't carry per-dist sizes, so we can't seed a
    // byte-total. Keep the bytes-side bar hidden and render a separate
    // "<done>/<total> packages" bar that ticks once per finished dist.
    let bar = DownloadBar::hidden();
    let total = dists.len() as u64;
    let pkg_bar = new_package_bar(total);
    // Two phases share one bar: download counts up to `total`, then we
    // reset to 0 and re-count for extraction. Without the reset the bar
    // would sit at 100% with a stale package name while extraction ran.
    let extract_started = std::sync::atomic::AtomicBool::new(false);
    let outcomes = fetch_and_extract_dists_with_progress(
        &client,
        paths,
        &dists,
        &bar,
        |name, _| {
            pkg_bar.set_message(name.to_owned());
            pkg_bar.inc(1);
        },
        |name| {
            if !extract_started.swap(true, std::sync::atomic::Ordering::AcqRel) {
                pkg_bar.set_prefix("extracting");
                pkg_bar.set_position(0);
            }
            pkg_bar.set_message(name.to_owned());
            pkg_bar.inc(1);
        },
    )?;
    pkg_bar.finish_and_clear();

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

    let bin_summary = super::bin_proxy::install_bin_proxies(project_root, &candidates);
    warnings.extend(bin_summary.warnings);

    let packages_installed = u32::try_from(
        outcomes
            .iter()
            .filter(|o| **o == DistOutcome::Downloaded)
            .count(),
    )
    .unwrap_or(u32::MAX);
    let packages_already_present = u32::try_from(
        outcomes
            .iter()
            .filter(|o| **o == DistOutcome::CacheHit)
            .count(),
    )
    .unwrap_or(u32::MAX);
    Ok(InstallSummary {
        project_root: project_root.to_path_buf(),
        packages_installed,
        packages_already_present,
        packages_skipped_plugin,
        bins_installed: bin_summary.bins_installed,
        no_dev: opts.no_dev,
        warnings,
    })
}

/// Build the per-package install progress bar. Renders on stderr when
/// progress is globally enabled (TTY, not `--quiet`, not JSON output)
/// and stays hidden otherwise — matching how `DownloadBar` gates its
/// own rendering. Length is the total dist count; callers tick once per
/// finished dist.
fn new_package_bar(total: u64) -> indicatif::ProgressBar {
    if !bougie_output::output::progress_visible() {
        let pb = indicatif::ProgressBar::hidden();
        pb.set_length(total);
        return pb;
    }
    let pb = indicatif::ProgressBar::new(total);
    pb.set_draw_target(indicatif::ProgressDrawTarget::stderr_with_hz(15));
    // No `{per_sec}`/`{eta}` here: package count is uniform-looking but
    // packages aren't uniform in size, so a per-package rate misleads
    // (a single Magento megapackage skews it) and the eta jitters wildly.
    let style = indicatif::ProgressStyle::with_template(
        "  {prefix:<12} {bar:32.magenta/white.dim} {pos}/{len} packages {msg}",
    )
    .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar())
    .progress_chars("--");
    pb.set_style(style);
    pb.set_prefix("downloading");
    pb.enable_steady_tick(std::time::Duration::from_millis(120));
    pb
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

/// Split lockfile contents into hard blockers (returned as `Err`) and
/// soft warnings (returned as `Ok`).
///
/// Hard blockers are things bougie genuinely cannot install:
/// source-only packages (no `dist`), non-zip dists, and missing
/// `dist.shasum`. The downstream loop in `install_from_lock` relies on
/// preflight having rejected these and unwraps accordingly.
///
/// Warnings are things bougie deliberately doesn't execute but can
/// install around: Composer plugins (the package zip is skipped — the
/// extracted tree would be inert without the install-time hook) and a
/// non-empty `scripts` section in `composer.json` (the package set
/// installs fine; the user's post-install hooks just don't run).
///
/// Every hard reason is aggregated into a single error so the user
/// sees every blocker in one pass rather than fix-one-hit-next.
fn preflight(composer_json_bytes: &[u8], lock: &Lock, no_dev: bool) -> Result<Vec<String>> {
    let mut reasons: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut plugin_packages: Vec<String> = Vec::new();

    // composer.json scripts → not run. Warn rather than fail; the
    // package install itself is unaffected. Users who depend on
    // post-install scripts (cache warm-up etc.) can still run them
    // explicitly afterwards via `bougie run -- composer run-script`.
    if let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(composer_json_bytes)
        && obj
            .get("scripts")
            .and_then(Value::as_object)
            .is_some_and(|s| !s.is_empty())
    {
        warnings.push(
            "composer.json declares `scripts` (post-install / post-autoload-dump etc.); \
             bougie does not run them. Invoke them manually with \
             `bougie run -- composer run-script <name>` if required."
                .into(),
        );
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
        if p.is_metapackage() {
            // Metapackages legitimately have no `dist` block — they
            // are pure require-graph aggregators. Nothing to install.
            continue;
        }
        if p.is_composer_plugin() {
            // Plugin install-time hooks are arbitrary PHP we won't
            // run. Skip the package — `install_from_lock` filters it
            // out of `install_set` for the same reason. Names are
            // aggregated into one warning after the loop.
            plugin_packages.push(p.name.clone());
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
        // Missing/empty `dist.shasum` is normal: every VCS-driver
        // dist (GitHub/GitLab/Bitbucket zipballs) emits an empty
        // shasum because the archive is server-generated and the
        // registry never sees the bytes. Composer treats empty/null
        // as skip-verify (FileDownloader.php:212); we do the same
        // and key the cache off `dist.reference` in that case (see
        // downloader::cache_path_for).
    }

    if !plugin_packages.is_empty() {
        let names = plugin_packages.join(", ");
        let noun = if plugin_packages.len() == 1 { "package" } else { "packages" };
        warnings.push(format!(
            "{noun} {names} {verb} Composer plugins (type `composer-plugin`); \
             bougie does not run plugin install-time hooks and skips \
             {pronoun}. Run `bougie run -- composer install` if the \
             plugin behavior is required.",
            verb = if plugin_packages.len() == 1 { "is a" } else { "are" },
            pronoun = if plugin_packages.len() == 1 { "the package itself" } else { "them" },
        ));
    }

    if reasons.is_empty() {
        Ok(warnings)
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
