//! `install_from_lock` — the orchestrator behind `bougie composer
//! install`.
//!
//! Reads `composer.json` + `composer.lock`, verifies content-hash,
//! diffs against the existing `vendor/composer/installed.json` to
//! determine which packages are already up-to-date, runs preflight
//! (rejecting source-only and non-zip dists which bougie cannot install
//! at all; surfacing plugins / post-install scripts as warnings since
//! bougie installs the package zips but never runs their PHP), builds
//! [`DistRequest`]s only for changed/new packages, removes stale
//! packages, calls [`fetch_and_extract_dists`] to populate `vendor/`,
//! then hands off to `bougie_autoloader::dump_autoload` to emit
//! `vendor/autoload.php` + `vendor/composer/installed.{json,php}`.
//!
//! Preflight failures are aggregated into a single error so the user
//! sees every blocker in one pass rather than fix-one-hit-next.
//! Preflight warnings are returned alongside on success and surfaced
//! to the user via [`InstallSummary::warnings`].

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use rustc_hash::FxHasher;

use bougie_autoloader::{dump_autoload, DumpRequest};
use bougie_composer::lockfile::{self, Lock, LockPackage};
use bougie_fetch::{ArchiveKind, DownloadBar};
use bougie_patches::{ApplyOptions, FailureMode, MaterializedPatch, PatchPlan, apply_patch_text};
use bougie_paths::Paths;
use eyre::{eyre, Context, Result};
use serde_json::Value;

use crate::metadata::AuthCredentials;
use crate::update::read_all_auth;

use super::downloader::{
    fetch_and_extract_dists_with_progress, DistCandidate, DistOutcome, DistRequest,
};

/// Caller-supplied install options. Mirrors the subset of Composer's
/// `install` flags we honor in Phase A.
#[derive(Debug, Clone, Copy, Default)]
pub struct InstallOptions {
    /// Skip packages in `composer.lock`'s `packages-dev` AND pass
    /// `no_dev=true` to the autoloader so dev autoload entries
    /// don't reach `vendor/autoload.php`.
    pub no_dev: bool,
}

/// Lifecycle hooks fired during an install when root-script execution is
/// opted in. The resolver stays command- and PHP-agnostic: the CLI supplies
/// an implementation that maps each hook to the right Composer event
/// (`pre-install-cmd` vs `pre-update-cmd`, …) and runs it via
/// `bougie-scripts`.
///
/// `Some(hooks)` is the scripts-ON gate: when present, the native Laravel
/// discovery + its drift guard are skipped (the real `post-autoload-dump`
/// entries run instead), and the "scripts not run" warning is suppressed.
/// `None` keeps the deterministic scripts-OFF behavior.
pub trait ScriptHooks {
    /// `pre-install-cmd` / `pre-update-cmd` — before package operations.
    fn pre_cmd(&self) -> Result<()>;
    /// `pre-autoload-dump` — before the autoloader is regenerated.
    fn pre_autoload_dump(&self) -> Result<()>;
    /// `post-autoload-dump` — after the autoloader is regenerated. Runs the
    /// project's real entries (e.g. Laravel's `package:discover`).
    fn post_autoload_dump(&self) -> Result<()>;
    /// `post-install-cmd` / `post-update-cmd` — at the very end.
    fn post_cmd(&self) -> Result<()>;
}

/// What happened. Returned to the CLI shim for `--format json-v1`
/// emission and rendered as a one-line text summary.
#[derive(Debug, Clone)]
pub struct InstallSummary {
    pub project_root: PathBuf,
    pub packages_installed: u32,
    pub packages_already_present: u32,
    /// Packages whose dist reference matched `installed.json` and whose
    /// vendor directory already existed — skipped entirely (no download,
    /// no extraction).
    pub packages_up_to_date: u32,
    /// Composer-plugin packages whose install-time hooks were not run.
    /// Their files install like any other package's — several plugins
    /// double as runtime libraries (`php-http/discovery` is the
    /// canonical case: its install hook is optional, but opensearch-php
    /// calls its `Http\Discovery\*` classes at runtime) — bougie just
    /// never executes their hook PHP.
    pub plugin_hooks_skipped: u32,
    /// Packages that were in the previous `installed.json` but are no
    /// longer in the lock file (or excluded by `--no-dev`) and had
    /// their vendor directory removed.
    pub packages_removed: u32,
    pub bins_installed: u32,
    /// Files copied into the project root by the native
    /// `magento/magento-composer-installer` deploy (`extra.map`). Zero
    /// for non-Magento projects.
    pub files_deployed: u64,
    pub no_dev: bool,
    /// On-disk bytes of the dist archives fetched this run (cache hits
    /// contribute nothing). Telemetry's `download_bytes`.
    pub download_bytes: u64,
    /// Wall-clock of the autoload dump, in milliseconds. `0` when the
    /// freshness marker let the dump be skipped entirely.
    pub autoload_ms: u64,
    /// Soft preflight findings — one entry per Composer plugin and one
    /// entry for a non-empty `scripts` section, plus any Magento deploy
    /// warnings. The CLI prints these as `warning: …` lines to stderr.
    pub warnings: Vec<String>,
}

/// Apply `composer.lock` to `project_root`. See module docs for the
/// flow.
pub fn install_from_lock(
    paths: &Paths,
    project_root: &Path,
    opts: InstallOptions,
    hooks: Option<&dyn ScriptHooks>,
) -> Result<InstallSummary> {
    install_from_lock_with_patches(paths, project_root, opts, hooks, None)
}

/// Like [`install_from_lock`], but also applies a resolved [`PatchPlan`]
/// (cweagans-style patches, reimplemented natively). `patches` is `None` for
/// the patches-disabled path, which keeps install-set diffing dist-ref-only
/// exactly as before.
///
/// The plan drives **two** touch points required for re-application
/// correctness (see `bougie-patches`): pre-extraction it forces every package
/// whose patch fingerprint changed into the install set (so a changed patch
/// set re-extracts pristine), and post-extraction it applies the patches and
/// rewrites `patches.lock.json`.
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
#[tracing::instrument(skip_all, fields(project_root = %project_root.display()))]
pub fn install_from_lock_with_patches(
    paths: &Paths,
    project_root: &Path,
    opts: InstallOptions,
    hooks: Option<&dyn ScriptHooks>,
    patches: Option<&PatchPlan>,
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

    // A stale lock (content-hash mismatch) warns rather than blocks —
    // same as Composer. Surface it first so it leads the warning list.
    let mut warnings = Vec::new();
    warnings.extend(content_hash_warning(&composer_json_bytes, &lock)?);
    warnings.extend(preflight(&composer_json_bytes, &lock, opts.no_dev, hooks.is_some())?);

    // Assemble per-host auth from every source bougie understands —
    // composer.json `config`, global `$COMPOSER_HOME/auth.json`,
    // project-level `auth.json`, and the `COMPOSER_AUTH` env var.
    // See `read_all_auth` for the precedence rationale. Dist URLs
    // sitting behind the same auth as the metadata (Magento's
    // `/archives/...`, private satis, GitLab CI Composer ZIPs) need
    // the header; public-CDN dists from Packagist do not.
    let composer_json_value: Value = serde_json::from_slice(&composer_json_bytes).map_err(|e| {
        bougie_errors::BougieError::Config { path: "composer.json".into(), detail: e.to_string() }
    })?;
    let auth: HashMap<String, AuthCredentials> =
        read_all_auth(&composer_json_value, project_root).map_err(|e| eyre!(e))?;

    // `pre-install-cmd` / `pre-update-cmd` — before any package operation
    // (download/extract). Only when the user opted into root scripts.
    if let Some(hooks) = hooks {
        hooks.pre_cmd()?;
    }

    // Gather the packages we'll actually install. Two filters:
    //   - `path` dists: skipped silently here. Preflight already
    //     rejected them when `opts` would make them install-time
    //     relevant, but a stray path-dist entry in a project that
    //     the user is comfortable with shouldn't block install — the
    //     autoloader treats them by reading the lock anyway.
    //   - metapackages: no `dist` and no code by definition; they
    //     exist purely as require-graph nodes.
    //
    // Composer-plugin packages install like any other (preflight warns
    // that their install-time hooks won't run): plugins are ordinary
    // packages whose code other packages may load at runtime —
    // `php-http/discovery` ships `Http\Discovery\*` classes that
    // opensearch-php calls — so skipping their files broke consumers
    // and left the autoloader with entries pointing nowhere.
    let candidates: Vec<&LockPackage> = if opts.no_dev {
        lock.packages.iter().collect()
    } else {
        lock.all_packages().collect()
    };
    let plugin_hooks_skipped = u32::try_from(
        candidates.iter().filter(|p| p.is_composer_plugin()).count(),
    )
    .unwrap_or(u32::MAX);

    // Drift guard for native Laravel discovery. bougie substitutes its own
    // package:discover + clearCompiled for Laravel's `post-autoload-dump`
    // script instead of running it. A custom step *after* the defaults is
    // fine (the defaults still run first). But a step *before* or *between*
    // them — or a renamed/removed discovery command — could change what the
    // defaults see, so bougie can't safely reproduce them: fail fast rather
    // than leave the app half-configured. Only applies when laravel/framework
    // is installed (the package that drives discovery).
    //
    // Skipped entirely when root scripts are opted in (`hooks.is_some()`):
    // the real `post-autoload-dump` entries run via the hook, so there's
    // nothing to reproduce and nothing to drift from.
    if hooks.is_none() && candidates.iter().any(|p| p.name == "laravel/framework") {
        let scripts = composer_json_value.get("scripts").cloned().unwrap_or(Value::Null);
        let blocking = bougie_installers::blocking_post_autoload_dump(&scripts);
        if !blocking.is_empty() {
            return Err(eyre!(
                "Laravel post-autoload-dump runs steps before/among the default Laravel steps \
                 that bougie does not reproduce:\n  {}\n\nbougie reproduces \
                 `artisan package:discover` and `ComposerScripts::postAutoloadDump` natively, \
                 but only when they run first — a step ahead of or between them may change what \
                 they see. Steps appended *after* the defaults are fine. Reorder so custom steps \
                 come last, or run the script yourself (`composer run-script post-autoload-dump`). \
                 If Laravel changed its default post-autoload-dump, bougie's native discovery is \
                 out of date — please report it.",
                blocking.join("\n  "),
            ));
        }
    }
    let installable: Vec<&LockPackage> = candidates
        .iter()
        .copied()
        .filter(|p| !p.is_path_dist() && !p.is_metapackage())
        .collect();

    // Path packages are materialized separately (symlink-or-copy), not
    // downloaded. Collect them up front so the stale-sweep in
    // `diff_install_set` knows to keep their vendor directories.
    let installer_paths = bougie_installers::InstallerPaths::parse(&composer_json_value);
    let path_packages: Vec<&LockPackage> =
        candidates.iter().copied().filter(|p| p.is_path_dist()).collect();
    let path_dests: Vec<PathBuf> = path_packages
        .iter()
        .map(|p| {
            let rel = bougie_installers::install_path(
                &p.name,
                p.package_type.as_deref(),
                &installer_paths,
            );
            project_root.join(rel)
        })
        .collect();
    let path_keep_names: HashSet<&str> =
        path_packages.iter().map(|p| p.name.as_str()).collect();

    // Diff against the existing installed state to skip packages whose
    // dist reference hasn't changed and whose vendor dir is still present.
    //
    // Patch-aware step: any package whose desired patch fingerprint differs
    // from the applied one (`patches.lock.json`) is forced back into the
    // install set even when its dist reference is unchanged — a patch is only
    // ever applied to a pristine tree, so a changed patch set must re-extract.
    // This runs *ahead* of the autoloader fast-path below, so a patch-only
    // change can't short-circuit to "Audited".
    let installed_state = read_installed_state(project_root);
    let force_patch: HashSet<&str> = patches
        .map(|plan| compute_force_set(plan, &installable, &installed_state, project_root))
        .unwrap_or_default();
    let (install_set, packages_up_to_date, packages_removed) =
        diff_install_set_with_force(
            &installable,
            &installed_state,
            project_root,
            &path_keep_names,
            &force_patch,
        );

    // Each DistRequest borrows from the LockPackage; build the
    // ancillary owned data (vendor dest paths, archive enums) in a
    // sibling vec so the borrows in `DistRequest` line up.
    //
    // Native `composer/installers`: a package's on-disk location can be
    // remapped by its `type` and the root `extra.installer-paths`. For
    // the common case (and every Magento 2 module) this resolves to
    // `vendor/<name>`. The same computation runs in `bougie-autoloader`
    // so the generated autoload + `installed.json` install-path point at
    // the relocated tree.
    let vendor_dirs: Vec<PathBuf> = install_set
        .iter()
        .map(|p| {
            let rel = bougie_installers::install_path(
                &p.name,
                p.package_type.as_deref(),
                &installer_paths,
            );
            // Surface composer/installers types bougie doesn't relocate:
            // the package lands in vendor/ and a framework expecting it
            // elsewhere won't find it. Only warn when it actually fell
            // back to vendor/<name> (a matching installer-paths override
            // would have relocated it, in which case there's no gap).
            if rel == format!("vendor/{}", p.name)
                && let Some(framework) =
                    bougie_installers::unsupported_framework(p.package_type.as_deref())
            {
                warnings.push(format!(
                    "{}: package type '{}' is a composer/installers '{}' type that bougie \
                     does not relocate — installing to vendor/{} (the framework may expect it \
                     elsewhere; add an extra.installer-paths entry to override)",
                    p.name,
                    p.package_type.as_deref().unwrap_or(""),
                    framework,
                    p.name,
                ));
            }
            project_root.join(rel)
        })
        .collect();
    // Expand each dist into its ordered candidate URL list —
    // `LockPackage::dist_urls()` puts a `preferred` mirror ahead of
    // the dist's own URL (Composer's mirror semantics; how a Private
    // Packagist dist downloads from the authenticated repo host
    // instead of the origin VCS host) — and pre-render each
    // candidate's `Authorization` header when its host matches the
    // auth map. Mirrors can sit on a different host than the dist
    // URL, so auth is resolved per candidate, not per package.
    // String storage lives in a sibling vec so `DistRequest` can
    // carry borrowed data — no per-request clones, no lifetime
    // gymnastics inside `par_iter`.
    let candidate_sets: Vec<Vec<DistCandidate>> = install_set
        .iter()
        .map(|p| {
            p.dist_urls()
                .into_iter()
                .map(|url| {
                    let auth_entry = auth_origin_from_url(&url)
                        .and_then(|origin| auth.get(origin))
                        .map(|creds| (creds.header_value(), creds.header_name()));
                    DistCandidate {
                        url,
                        auth_header: auth_entry.as_ref().map(|(v, _)| v.clone()),
                        auth_header_name: auth_entry.as_ref().map(|(_, n)| *n),
                    }
                })
                .collect()
        })
        .collect();
    let dists: Vec<DistRequest<'_>> = install_set
        .iter()
        .zip(vendor_dirs.iter())
        .zip(candidate_sets.iter())
        .map(|((p, dest), candidates)| {
            let dist = p.dist.as_ref().unwrap();
            // `dist_urls()` yields at least the dist's own URL for
            // every package with a dist, and preflight filtered the
            // rest out of the install set.
            let (primary, fallbacks) =
                candidates.split_first().expect("non-empty dist_urls for a dist package");
            DistRequest {
                package_name: &p.name,
                url: &primary.url,
                sha1: dist.shasum.as_deref().unwrap_or(""),
                reference: dist.reference.as_deref().unwrap_or(""),
                archive: ArchiveKind::Zip,
                strip_prefix: None,
                vendor_dest: dest,
                auth_header: primary.auth_header.as_deref(),
                auth_header_name: primary.auth_header_name,
                project_root,
                fallbacks,
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

    // Materialize `type: path` packages (symlink-or-copy) into their
    // vendor destinations. Done before the autoload dump so the
    // generated autoloader sees the linked trees.
    let path_summary = super::path_link::materialize_path_packages(
        project_root,
        &path_packages,
        &path_dests,
        installed_state.as_ref(),
    );
    warnings.extend(path_summary.warnings);

    // Native patch application. Every `install_set` member is pristine here
    // (freshly extracted), so this is the only safe place to patch. Must
    // run *before* the Magento deploy below so `extra.map`-copied files
    // carry the patched content — Composer's ordering, where cweagans
    // patches apply per package at install time and the
    // magento-composer-installer deploy runs at the end of the command.
    if let Some(plan) = patches {
        apply_patch_plan(plan, &install_set, &vendor_dirs, project_root, &mut warnings)?;
    }

    // Native `magento/magento-composer-installer`: for every
    // freshly-extracted Magento 2 component, copy its `extra.map` files
    // into the project root and apply `extra.chmod`. Driving this off
    // `install_set` (only changed/new packages) matches Composer, which
    // runs the deploy on package install/update — not on every `install`
    // when nothing changed. A deploy failure is surfaced as a warning
    // rather than aborting the whole sync (a single third-party package's
    // unsupported map shouldn't block a Magento project).
    let deploy_summary = deploy_components(&install_set, &vendor_dirs, project_root);
    warnings.extend(deploy_summary.warnings);

    // `pre-autoload-dump` — before the autoloader is regenerated.
    if let Some(hooks) = hooks {
        hooks.pre_autoload_dump()?;
    }

    // Autoloader freshness fast-path — the uv "Audited" shape. The
    // non-optimized autoloader is a pure function of the root manifest
    // (autoload/config/extra) and the lock (the full package set plus
    // each package's own autoload block). When nothing was installed or
    // removed this run, that fingerprint is unchanged, and the output is
    // still on disk, a regenerated tree would be byte-identical — so the
    // ~200ms dump is pure waste and we skip it. Restricted to the
    // scripts-off default: with root scripts on we keep the
    // unconditional dump so the `pre`/`post-autoload-dump` lifecycle is
    // observable exactly as before.
    let autoload_marker = project_root
        .join("vendor/composer")
        .join(AUTOLOAD_FRESH_MARKER);
    let lock_bytes = std::fs::read(&composer_lock_path).unwrap_or_default();
    let fingerprint = autoload_fingerprint(&composer_json_bytes, &lock_bytes, opts.no_dev).to_string();
    let autoload_fresh = hooks.is_none()
        && install_set.is_empty()
        && packages_removed == 0
        && path_summary.linked == 0
        && project_root.join("vendor/autoload.php").is_file()
        && std::fs::read_to_string(&autoload_marker)
            .ok()
            .as_deref()
            .map(str::trim)
            == Some(fingerprint.as_str());

    let autoload_started = std::time::Instant::now();
    if !autoload_fresh {
        dump_autoload(&DumpRequest {
            project_root,
            optimize: false,
            classmap_authoritative: false,
            no_dev: opts.no_dev,
            apcu_autoloader: false,
            apcu_prefix: None,
            autoloader_suffix: None,
        })
        .wrap_err("autoload dump failed")?;
        // Record the fingerprint so the next unchanged sync can skip the
        // dump. Best-effort: a write failure just means we regenerate
        // next time (the old behavior), so it's a warning, not an error.
        if let Err(e) = std::fs::write(&autoload_marker, &fingerprint) {
            warnings.push(format!(
                "could not write autoload freshness marker {}: {e}",
                autoload_marker.display()
            ));
        }
    }
    let autoload_ms =
        u64::try_from(autoload_started.elapsed().as_millis()).unwrap_or(u64::MAX);

    match hooks {
        // Scripts ON: run the project's real `post-autoload-dump` entries.
        // Laravel's `@php artisan package:discover` writes `packages.php`
        // itself and the `ComposerScripts::postAutoloadDump` callback is
        // served by a native handler (clearCompiled) — no double-write.
        Some(hooks) => hooks.post_autoload_dump()?,
        // Scripts OFF: native Laravel package discovery — the effect of
        // Laravel's `post-autoload-dump` script (`artisan package:discover`
        // + `ComposerScripts::postAutoloadDump`), which bougie won't run.
        // Gated on `laravel/framework` being installed. Runs after the
        // autoload dump because it reads the freshly-written `installed.json`.
        None => {
            if candidates.iter().any(|p| p.name == "laravel/framework")
                && let Err(e) = run_laravel_discovery(project_root, &composer_json_value)
            {
                warnings.push(format!("laravel package discovery failed: {e}"));
            }
        }
    }

    let bin_summary = super::bin_proxy::install_bin_proxies(project_root, &candidates);
    warnings.extend(bin_summary.warnings);

    // `post-install-cmd` / `post-update-cmd` — at the very end, after bins.
    if let Some(hooks) = hooks {
        hooks.post_cmd()?;
    }

    let packages_installed = u32::try_from(
        outcomes
            .iter()
            .filter(|o| matches!(o, DistOutcome::Downloaded { .. }))
            .count(),
    )
    .unwrap_or(u32::MAX);
    let download_bytes: u64 = outcomes
        .iter()
        .map(|o| match o {
            DistOutcome::Downloaded { bytes } => *bytes,
            DistOutcome::CacheHit => 0,
        })
        .sum();
    let packages_already_present = u32::try_from(
        outcomes
            .iter()
            .filter(|o| **o == DistOutcome::CacheHit)
            .count(),
    )
    .unwrap_or(u32::MAX);
    // Fold path-package materialization into the totals: a freshly
    // linked/copied path package counts as installed; an unchanged one
    // counts as up-to-date.
    let packages_installed = packages_installed.saturating_add(path_summary.linked);
    let packages_up_to_date = packages_up_to_date.saturating_add(path_summary.up_to_date);
    Ok(InstallSummary {
        project_root: project_root.to_path_buf(),
        packages_installed,
        packages_already_present,
        packages_up_to_date,
        plugin_hooks_skipped,
        packages_removed,
        bins_installed: bin_summary.bins_installed,
        files_deployed: deploy_summary.files_deployed,
        no_dev: opts.no_dev,
        download_bytes,
        autoload_ms,
        warnings,
    })
}

/// The set of packages to force back into the install set for patching: those
/// whose patch fingerprint changed, plus — for project-root patches — every
/// package coupled to one already being re-extracted.
///
/// A root ("top-level") patch is applied atomically at the project root, so
/// its packages must stay in lockstep: if any one is re-extracted this run
/// (its dist changed, or another patch forces it), all of them must be
/// re-extracted together and the patch re-applied — otherwise the siblings
/// that stayed on disk would be patched twice. The coupling is expanded to a
/// fixpoint because root patches can chain through a shared package.
fn compute_force_set<'a>(
    plan: &'a PatchPlan,
    installable: &[&'a LockPackage],
    installed_state: &Option<InstalledState>,
    project_root: &Path,
) -> HashSet<&'a str> {
    // Seed: packages whose patch fingerprint changed since the applied state.
    let mut force: HashSet<&str> = plan
        .tracked_packages()
        .filter(|name| plan.fingerprint_changed(name))
        .collect();

    if plan.root_patches.is_empty() {
        return force;
    }

    let by_name: HashMap<&str, &LockPackage> =
        installable.iter().map(|p| (p.name.as_str(), *p)).collect();
    // Whether a package needs a fresh extract for a non-patch reason (its dist
    // reference changed, its vendor dir is gone, or there is no prior state).
    let needs_reextract = |name: &str| -> bool {
        by_name.get(name).is_some_and(|p| {
            installed_state
                .as_ref()
                .is_none_or(|state| !dist_up_to_date(p, state, project_root))
        })
    };

    loop {
        let mut added = false;
        for rp in &plan.root_patches {
            let any_reextract = rp
                .packages
                .iter()
                .any(|pkg| force.contains(pkg.as_str()) || needs_reextract(pkg));
            if any_reextract {
                for pkg in &rp.packages {
                    // Only force packages actually installable this run; a
                    // non-installable one can't be re-extracted regardless.
                    if let Some(p) = by_name.get(pkg.as_str())
                        && force.insert(p.name.as_str())
                    {
                        added = true;
                    }
                }
            }
        }
        if !added {
            break;
        }
    }
    force
}

/// Apply a resolved [`PatchPlan`] to the freshly-extracted `install_set`,
/// then rewrite `patches.lock.json` (always — it is load-bearing for the next
/// run's diff). Package-scoped patches apply into each target's install dir;
/// project-root patches apply once at the project root. Every re-extracted
/// tracked package then earns the desired fingerprint (fully applied) or loses
/// it (partial apply, or no longer patched — the cleanup transition). Returns
/// `Err` only in `Abort` failure mode; otherwise apply failures surface as
/// `warnings`.
fn apply_patch_plan(
    plan: &PatchPlan,
    install_set: &[&LockPackage],
    vendor_dirs: &[PathBuf],
    project_root: &Path,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let mut new_fingerprints = plan.applied.clone();
    let install_names: HashSet<&str> = install_set.iter().map(|p| p.name.as_str()).collect();

    // Packages whose patch set did not fully apply this run. They must not earn
    // a fingerprint, so the next run re-extracts pristine and retries.
    let mut failed: HashSet<String> = HashSet::new();

    // 1. Package-scoped patches: apply into each package's install dir.
    for (pkg, vendor_dir) in install_set.iter().zip(vendor_dirs.iter()) {
        let Some(pkg_patches) = plan.patches.get(&pkg.name) else {
            continue;
        };
        let res = bougie_patches::apply_package_patches(
            vendor_dir,
            &pkg.name,
            pkg_patches,
            plan.failure_mode,
            plan.skip_report,
        )?;
        warnings.extend(res.warnings);
        if res.fingerprint.is_none() {
            failed.insert(pkg.name.clone());
        }
    }

    // 2. Project-root patches: apply once each, at the project root. Coupling
    // guarantees a root patch's packages are all-in or all-out of the install
    // set, so `all_pristine` distinguishes "re-extracted this run, (re)apply
    // now" from "untouched, already applied before".
    let vendor_by_name: HashMap<&str, &PathBuf> = install_set
        .iter()
        .zip(vendor_dirs.iter())
        .map(|(p, dir)| (p.name.as_str(), dir))
        .collect();
    for rp in &plan.root_patches {
        let all_pristine = rp.packages.iter().all(|p| install_names.contains(p.as_str()));
        if !all_pristine {
            continue;
        }
        if apply_root_patch(project_root, &rp.patch, plan.failure_mode, warnings)? {
            // Record the patch in each touched package's `PATCHES.txt` so every
            // patched directory carries the same audit trail as a package-scoped
            // patch (the package's own `PATCHES.txt`, if any, is appended to).
            if !plan.skip_report {
                for pkg in &rp.packages {
                    if let Some(dir) = vendor_by_name.get(pkg.as_str())
                        && let Err(e) = bougie_patches::append_patches_txt(dir, &[&rp.patch])
                    {
                        warnings.push(format!("could not update PATCHES.txt in `{}`: {e}", dir.display()));
                    }
                }
            }
        } else {
            failed.extend(rp.packages.iter().cloned());
        }
    }

    // 3. Recompute fingerprints for every tracked package re-extracted this run.
    // Untouched packages keep their prior fingerprint.
    for name in plan.tracked_packages() {
        if !install_names.contains(name) {
            continue;
        }
        match (failed.contains(name), plan.desired_fingerprint(name)) {
            // Fully applied → record the combined (package + root) fingerprint.
            (false, Some(fp)) => {
                new_fingerprints.insert(name.to_string(), fp);
            }
            // Partial apply, or no longer patched → drop, so the next run
            // re-extracts pristine (and retries any failed patch).
            _ => {
                new_fingerprints.remove(name);
            }
        }
    }

    let human = plan.write_lock.then(|| plan.human_view());
    if let Err(e) = bougie_patches::lock::write_with_human(project_root, &new_fingerprints, human) {
        warnings.push(format!("could not write patches.lock.json: {e}"));
    }
    Ok(())
}

/// Apply a single project-root ("top-level") patch at the project root
/// (the caller records it in each touched package's `PATCHES.txt`). Returns
/// whether it applied cleanly: in `Abort` mode a failure is an `Err`; in
/// `SkipAndWarn` it pushes a warning and returns `Ok(false)`.
fn apply_root_patch(
    project_root: &Path,
    patch: &MaterializedPatch,
    failure_mode: FailureMode,
    warnings: &mut Vec<String>,
) -> Result<bool> {
    let text = std::fs::read_to_string(&patch.local_path)
        .with_context(|| format!("reading patch `{}`", patch.local_path.display()))?;
    let opts = ApplyOptions { depth: patch.depth, ..ApplyOptions::default() };
    match apply_patch_text(project_root, &text, &opts) {
        Ok(_) => Ok(true),
        Err(e) => match failure_mode {
            FailureMode::Abort => Err(eyre!(
                "patch `{}` failed to apply at the project root: {e:#}",
                patch.description
            )),
            FailureMode::SkipAndWarn => {
                warnings.push(format!(
                    "patch `{}` failed to apply at the project root: {e:#}",
                    patch.description
                ));
                Ok(false)
            }
        },
    }
}

/// Outcome of the native Magento deploy pass.
struct DeploySummary {
    files_deployed: u64,
    warnings: Vec<String>,
}

/// Run the native `magento/magento-composer-installer` deploy over the
/// freshly-extracted packages. `install_set[i]` was extracted to
/// `vendor_dirs[i]`, so the two slices are zipped. For each Magento 2
/// component we copy its `extra.map` into `project_root` and apply
/// `extra.chmod`; if any `magento2-component` was deployed we emit
/// `app/etc/vendor_path.php` (which the plugin generates rather than
/// maps). Deploy failures become warnings — a malformed map in one
/// package shouldn't abort the whole install.
fn deploy_components(
    install_set: &[&LockPackage],
    vendor_dirs: &[PathBuf],
    project_root: &Path,
) -> DeploySummary {
    let mut files_deployed = 0u64;
    let mut warnings = Vec::new();
    let mut deployed_component = false;

    for (pkg, vendor_dir) in install_set.iter().zip(vendor_dirs.iter()) {
        let Some(plan) = bougie_installers::plan_deploy(pkg.package_type.as_deref(), &pkg.extra)
        else {
            continue;
        };
        if plan.map.is_empty() && plan.chmod.is_empty() {
            continue;
        }
        match bougie_installers::apply_deploy(&plan, vendor_dir, project_root) {
            Ok(stats) => {
                files_deployed += stats.files_copied;
                deployed_component |= plan.is_component;
            }
            Err(e) => {
                warnings.push(format!("deploy of {} failed: {e}", pkg.name));
            }
        }
    }

    // `app/etc/vendor_path.php` is generated by the installer (it is not
    // one of magento2-base's mapped files); Magento's bootstrap reads it
    // to locate `vendor/`. Emit it once whenever a component laid down
    // the root skeleton.
    if deployed_component
        && let Err(e) = write_vendor_path_php(project_root)
    {
        warnings.push(format!("writing app/etc/vendor_path.php failed: {e}"));
    }

    DeploySummary { files_deployed, warnings }
}

/// Reproduce Laravel's package discovery: rebuild
/// `bootstrap/cache/packages.php` from `installed.json` + the root
/// `extra.laravel`, then clear the stale compiled caches Laravel's
/// `clearCompiled()` removes. Mirrors what `artisan package:discover`
/// (run via `post-autoload-dump`) would do.
fn run_laravel_discovery(project_root: &Path, composer_json_value: &Value) -> Result<()> {
    let installed_path = project_root.join("vendor/composer/installed.json");
    let bytes = std::fs::read(&installed_path)
        .wrap_err_with(|| format!("reading {}", installed_path.display()))?;
    let installed: Value =
        serde_json::from_slice(&bytes).wrap_err("parsing installed.json")?;
    let root_extra = composer_json_value.get("extra").cloned().unwrap_or(Value::Null);

    let manifest = bougie_installers::build_package_manifest(&installed, &root_extra);
    let php = bougie_installers::render_packages_php(&manifest);

    let cache_path = project_root.join(bougie_installers::PACKAGES_CACHE);
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)
            .wrap_err_with(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&cache_path, php)
        .wrap_err_with(|| format!("writing {}", cache_path.display()))?;

    // clearCompiled(): drop the config/services caches so they can't
    // reference a package set that just changed.
    for rel in bougie_installers::STALE_CACHES {
        let path = project_root.join(rel);
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

/// Write `app/etc/vendor_path.php` (creating `app/etc/` if needed).
fn write_vendor_path_php(project_root: &Path) -> Result<()> {
    let dir = project_root.join("app/etc");
    std::fs::create_dir_all(&dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join("vendor_path.php"), bougie_installers::VENDOR_PATH_PHP)
        .wrap_err("writing vendor_path.php")?;
    Ok(())
}

/// Snapshot of `vendor/composer/installed.json` — the packages and
/// dist references that were installed on the previous run.
pub(crate) struct InstalledState {
    /// Package name → dist reference from the previous install.
    pub(crate) packages: HashMap<String, String>,
}

/// Read `vendor/composer/installed.json` and extract the package name →
/// dist reference mapping. Returns `None` if the file doesn't exist or
/// can't be parsed (first install, corrupted state, etc.).
fn read_installed_state(project_root: &Path) -> Option<InstalledState> {
    let path = project_root.join("vendor/composer/installed.json");
    let bytes = std::fs::read(&path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let obj = value.as_object()?;

    let packages_arr = obj.get("packages")?.as_array()?;
    let mut packages = HashMap::with_capacity(packages_arr.len());
    for pkg in packages_arr {
        let name = pkg.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let reference = pkg
            .get("dist")
            .and_then(|d| d.get("reference"))
            .and_then(|r| r.as_str())
            .unwrap_or("");
        packages.insert(name.to_string(), reference.to_string());
    }

    Some(InstalledState { packages })
}

/// Diff the lock file's installable set against the existing
/// `installed.json` state. Returns:
/// - the subset of packages that actually need downloading/extracting,
/// - the count of packages that are already up-to-date,
/// - the count of stale packages whose vendor dirs were removed.
#[cfg(test)]
pub(crate) fn diff_install_set<'a>(
    installable: &[&'a LockPackage],
    installed_state: &Option<InstalledState>,
    project_root: &Path,
    keep_names: &HashSet<&str>,
) -> (Vec<&'a LockPackage>, u32, u32) {
    diff_install_set_with_force(
        installable,
        installed_state,
        project_root,
        keep_names,
        &HashSet::new(),
    )
}

/// Like [`diff_install_set`], but `force` names packages that must be
/// (re-)installed even when their dist reference matches and the vendor dir is
/// present — the patch-aware path, used to re-extract a package pristine when
/// its patch fingerprint changed. `keep_names` are packages materialized by
/// another path (e.g. `type: path` repositories) whose vendor dirs must not be
/// swept as stale.
pub(crate) fn diff_install_set_with_force<'a>(
    installable: &[&'a LockPackage],
    installed_state: &Option<InstalledState>,
    project_root: &Path,
    keep_names: &HashSet<&str>,
    force: &HashSet<&str>,
) -> (Vec<&'a LockPackage>, u32, u32) {
    let Some(state) = installed_state else {
        return (installable.to_vec(), 0, 0);
    };

    let mut need_install: Vec<&'a LockPackage> = Vec::new();
    let mut up_to_date: u32 = 0;
    // `keep_names` are packages installed by another path (path
    // repositories materialize into `vendor/` separately) — their
    // directories must not be swept as stale here.
    let mut wanted_names: HashSet<&str> = installable.iter().map(|p| p.name.as_str()).collect();
    wanted_names.extend(keep_names.iter().copied());

    for p in installable {
        if !force.contains(p.name.as_str()) && dist_up_to_date(p, state, project_root) {
            up_to_date = up_to_date.saturating_add(1);
            continue;
        }
        need_install.push(p);
    }

    // Remove stale packages: present in the old installed state but
    // absent from the current lock's installable set.
    let mut removed: u32 = 0;
    for old_name in state.packages.keys() {
        if !wanted_names.contains(old_name.as_str()) {
            let vendor_dir = project_root.join("vendor").join(old_name);
            if vendor_dir.is_dir() {
                let _ = std::fs::remove_dir_all(&vendor_dir);
                removed = removed.saturating_add(1);
            }
        }
    }

    (need_install, up_to_date, removed)
}

/// Whether `p` is already installed at the locked revision: its recorded dist
/// reference matches the lock and its `vendor/<name>` directory is present.
/// The single source of truth for "no fresh extract needed", shared by the
/// install-set diff and the patch-coupling force computation.
fn dist_up_to_date(p: &LockPackage, state: &InstalledState, project_root: &Path) -> bool {
    let lock_ref = p
        .dist
        .as_ref()
        .and_then(|d| d.reference.as_deref())
        .unwrap_or("");
    state.packages.get(&p.name).is_some_and(|installed_ref| {
        installed_ref == lock_ref && project_root.join("vendor").join(&p.name).is_dir()
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

/// Extract the credential-lookup key for a dist URL — Composer's
/// origin: the authority between `://` and the next `/`, INCLUDING any
/// `:port` suffix (`127.0.0.1:8080`, `repo.magento.com`). Returns
/// `None` for URLs without a parseable host (e.g. file URIs in tests).
/// Keys per-host auth lookup the same way the metadata path does (see
/// [`crate::metadata::auth_origin`]) — and the same way Composer does:
/// the port is part of the key, so a mirror on a non-default port
/// matches its `auth.json` entry. Path differences inside the host
/// don't matter (everything under the same authority shares
/// credentials whether the URL targets `/p2/...` for metadata or
/// `/archives/...` for a dist).
fn auth_origin_from_url(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://")?.1;
    let host_and_port = after_scheme.split('/').next()?;
    if host_and_port.is_empty() { None } else { Some(host_and_port) }
}

/// Check the lock's `content-hash` field against the current
/// Marker file (under `vendor/composer/`) holding the autoloader
/// freshness fingerprint from the last dump. Its presence + match lets
/// a subsequent unchanged sync skip the autoloader regeneration.
const AUTOLOAD_FRESH_MARKER: &str = ".bougie-autoload-fresh";

/// Fingerprint of everything the non-optimized autoloader output
/// depends on: the root manifest bytes (autoload / config /
/// installer-paths in `extra`) and the lock bytes (the full package set
/// and each package's own `autoload` block), plus the dev flag.
///
/// Non-cryptographic on purpose — this is a local freshness cache, not
/// a trust boundary, and the crate already prefers fast hashing over
/// pulling `sha2` for non-security work. A hash collision would only
/// cost a skipped regeneration, recoverable with an explicit
/// `bougie composer dump-autoloader`.
fn autoload_fingerprint(composer_json_bytes: &[u8], lock_bytes: &[u8], no_dev: bool) -> u64 {
    let mut h = FxHasher::default();
    composer_json_bytes.hash(&mut h);
    lock_bytes.hash(&mut h);
    no_dev.hash(&mut h);
    h.finish()
}

/// `composer.json` bytes, using the same algorithm Composer itself
/// runs (delegated to `bougie_composer::lockfile::content_hash`).
///
/// A mismatch is *not* a hard failure: Composer only warns ("The lock
/// file is not up to date with the latest changes in composer.json")
/// and installs the lock's package set anyway, so the user gets a
/// reproducible (if slightly stale) environment rather than a refusal.
/// We mirror that — return the warning string on mismatch, `None`
/// otherwise — and the caller folds it into `InstallSummary::warnings`.
fn content_hash_warning(composer_json_bytes: &[u8], lock: &Lock) -> Result<Option<String>> {
    let Some(expected) = &lock.content_hash else {
        // Pre-1.10 lockfiles don't carry a content-hash. Composer
        // tolerates them; we do too rather than refuse to install a
        // perfectly working historical project.
        return Ok(None);
    };
    let actual = lockfile::content_hash(composer_json_bytes)?;
    if actual.eq_ignore_ascii_case(expected) {
        return Ok(None);
    }
    Ok(Some(format!(
        "composer.lock is out of sync with composer.json (content-hash {expected} → {actual}); \
         installing the locked package set anyway — you may be getting outdated dependencies. \
         Run `bougie run -- composer update` to regenerate the lock.",
    )))
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
/// install around: Composer plugins (their files install like any other
/// package's — several double as runtime libraries — but their
/// install-time hooks never run) and a non-empty `scripts` section in
/// `composer.json` when script execution isn't opted in (the package set
/// installs fine; the user's post-install hooks just don't run). When
/// `scripts_on`, the hooks run the scripts, so no warning is emitted.
///
/// Every hard reason is aggregated into a single error so the user
/// sees every blocker in one pass rather than fix-one-hit-next.
fn preflight(
    composer_json_bytes: &[u8],
    lock: &Lock,
    no_dev: bool,
    scripts_on: bool,
) -> Result<Vec<String>> {
    let mut reasons: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut plugin_packages: Vec<String> = Vec::new();

    // composer.json scripts → not run unless opted in. When scripts are
    // off, warn — but only about scripts bougie doesn't already reproduce
    // natively (Laravel's discovery `post-autoload-dump` runs either way).
    // The warning advertises the opt-in rather than the old run-script
    // workaround. When scripts are on, the hooks run them: no warning.
    if !scripts_on
        && let Ok(value) = serde_json::from_slice::<Value>(composer_json_bytes)
        && value
            .get("scripts")
            .and_then(Value::as_object)
            .is_some_and(|s| !s.is_empty())
        && !bougie_installers::only_reproduced_scripts(value.get("scripts").unwrap_or(&Value::Null))
    {
        warnings.push(
            "composer.json declares `scripts`; bougie does not run them by default. \
             Enable with `[scripts] run = true` in bougie.toml or pass `--scripts`."
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
            // Plugin install-time hooks are arbitrary PHP we won't run,
            // but the package's FILES install like any other (plugins
            // can double as runtime libraries — php-http/discovery's
            // classes are called by opensearch-php at runtime). Collect
            // the name for one aggregated hooks-not-run warning after
            // the loop, then fall through to the dist checks below:
            // since we extract it, its dist must be installable.
            plugin_packages.push(p.name.clone());
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
             {pronoun} files install like any other package's, but bougie \
             does not run plugin install-time hooks. Run \
             `bougie run -- composer install` if the hook behavior is \
             required.",
            verb = if plugin_packages.len() == 1 { "is a" } else { "are" },
            pronoun = if plugin_packages.len() == 1 { "its" } else { "their" },
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
