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
    /// Packages whose dist reference matched `installed.json` and whose
    /// vendor directory already existed — skipped entirely (no download,
    /// no extraction).
    pub packages_up_to_date: u32,
    /// Composer-plugin packages we skipped over (their zip was not
    /// extracted because bougie won't run plugin install-time PHP and
    /// the extracted tree would be inert).
    pub packages_skipped_plugin: u32,
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
    /// Soft preflight findings — one entry per Composer plugin and one
    /// entry for a non-empty `scripts` section, plus any Magento deploy
    /// warnings. The CLI prints these as `warning: …` lines to stderr.
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
#[tracing::instrument(skip_all, fields(project_root = %project_root.display()))]
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

    // Drift guard for native Laravel discovery. bougie substitutes its own
    // package:discover + clearCompiled for Laravel's `post-autoload-dump`
    // script instead of running it. A custom step *after* the defaults is
    // fine (the defaults still run first). But a step *before* or *between*
    // them — or a renamed/removed discovery command — could change what the
    // defaults see, so bougie can't safely reproduce them: fail fast rather
    // than leave the app half-configured. Only applies when laravel/framework
    // is installed (the package that drives discovery).
    if candidates.iter().any(|p| p.name == "laravel/framework") {
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
        .filter(|p| !p.is_path_dist() && !p.is_composer_plugin() && !p.is_metapackage())
        .collect();

    // Diff against the existing installed state to skip packages whose
    // dist reference hasn't changed and whose vendor dir is still present.
    let installed_state = read_installed_state(project_root);
    let (install_set, packages_up_to_date, packages_removed) =
        diff_install_set(&installable, &installed_state, project_root);

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
    let installer_paths = bougie_installers::InstallerPaths::parse(&composer_json_value);
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
                project_root,
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

    // Native Laravel package discovery — the effect of Laravel's
    // `post-autoload-dump` script (`artisan package:discover` +
    // `ComposerScripts::postAutoloadDump`), which bougie won't run.
    // Gated on `laravel/framework` being installed (the package whose
    // script would otherwise drive discovery). Runs after the autoload
    // dump because it reads the freshly-written `installed.json`.
    if candidates.iter().any(|p| p.name == "laravel/framework")
        && let Err(e) = run_laravel_discovery(project_root, &composer_json_value)
    {
        warnings.push(format!("laravel package discovery failed: {e}"));
    }

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
        packages_up_to_date,
        packages_skipped_plugin,
        packages_removed,
        bins_installed: bin_summary.bins_installed,
        files_deployed: deploy_summary.files_deployed,
        no_dev: opts.no_dev,
        warnings,
    })
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
pub(crate) fn diff_install_set<'a>(
    installable: &[&'a LockPackage],
    installed_state: &Option<InstalledState>,
    project_root: &Path,
) -> (Vec<&'a LockPackage>, u32, u32) {
    let Some(state) = installed_state else {
        return (installable.to_vec(), 0, 0);
    };

    let mut need_install: Vec<&'a LockPackage> = Vec::new();
    let mut up_to_date: u32 = 0;
    let wanted_names: HashSet<&str> = installable.iter().map(|p| p.name.as_str()).collect();

    for p in installable {
        let lock_ref = p
            .dist
            .as_ref()
            .and_then(|d| d.reference.as_deref())
            .unwrap_or("");
        let vendor_dir = project_root.join("vendor").join(&p.name);

        if let Some(installed_ref) = state.packages.get(&p.name) {
            if installed_ref == lock_ref && vendor_dir.is_dir() {
                up_to_date = up_to_date.saturating_add(1);
                continue;
            }
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
