//! `bougie composer update` (aliases `upgrade` / `u`) — resolve
//! `composer.json`, write a fresh `composer.lock`, and install the
//! result into `vendor/`, matching Composer's `update`.
//!
//! `--dry-run` previews the solution and writes nothing; `--no-install`
//! stops after writing the lock (Composer's flag). Without either, it
//! resolves, writes the lock atomically, then materializes `vendor/`
//! via `install_from_lock`.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer::lockfile::{self, canonical_readme, Lock};
use bougie_composer_resolver::metadata::Repo;
use bougie_composer_resolver::{
    dry_run_update, dry_run_update_partial, resolve_for_lockfile_partial,
    DryRunOptions, InstallOptions, LockfileSolveOutcome, PartialUpdate, PlatformIgnore,
    ResolutionStrategy, ResolvedPackage, UpdateSummary,
};
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use eyre::{Context, Result};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct UpdateResult {
    pub schema_version: u32,
    pub project_root: PathBuf,
    pub no_dev: bool,
    pub dry_run: bool,
    pub packages: Vec<ResolvedPackage>,
    pub packages_dev: Vec<ResolvedPackage>,
    /// When `dry_run = false`, the path the lockfile was written to.
    /// Omitted from the dry-run output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_path: Option<PathBuf>,
    /// Packages freshly downloaded into `vendor/` by the post-lock
    /// install. `None` when the install was skipped (`--no-install` /
    /// `--dry-run`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packages_installed: Option<u32>,
    /// Packages already present in `vendor/` (cache hits). `None` when
    /// the install was skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packages_already_present: Option<u32>,
}

impl Render for UpdateResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let total = self.packages.len() + self.packages_dev.len();
        if total == 0 {
            writeln!(
                w,
                "composer update: no packages to install (composer.json has no external requires)",
            )?;
            return Ok(());
        }
        let mode_dev = if self.no_dev { " (no-dev)" } else { "" };
        let mode_dry = if self.dry_run { " --dry-run" } else { "" };
        writeln!(
            w,
            "composer update{mode_dry}{mode_dev}: {} packages ({} prod, {} dev)",
            total,
            self.packages.len(),
            self.packages_dev.len(),
        )?;
        for p in &self.packages {
            writeln!(w, "  {} {}", p.name, p.version)?;
        }
        for p in &self.packages_dev {
            writeln!(w, "  {} {} (dev)", p.name, p.version)?;
        }
        if self.dry_run {
            writeln!(w)?;
            writeln!(
                w,
                "(read-only preview — pass without --dry-run to write composer.lock)",
            )?;
        } else if let Some(path) = &self.lock_path {
            writeln!(w)?;
            writeln!(w, "wrote {}", path.display())?;
            match (self.packages_installed, self.packages_already_present) {
                (Some(fresh), Some(cached)) => {
                    writeln!(w, "installed into vendor/ ({fresh} fresh, {cached} cached)")?;
                }
                _ => writeln!(w, "(--no-install: vendor/ not touched)")?,
            }
        }
        Ok(())
    }
}

#[allow(
    clippy::fn_params_excessive_bools,
    clippy::too_many_arguments,
    clippy::needless_pass_by_value
)]
pub fn run(
    format: OutputFormat,
    working_dir: Option<PathBuf>,
    no_dev: bool,
    dry_run: bool,
    no_install: bool,
    packages: Vec<String>,
    with_dependencies: bool,
    with_all_dependencies: bool,
    resolution: ResolutionStrategy,
    ignore_platform: PlatformIgnore,
) -> Result<ExitCode> {
    let project_root = match working_dir {
        Some(p) => p,
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };
    let paths = Paths::from_env()?;

    // Partial update (`composer update <pkg>...`): hold every other
    // package at its locked version. Requires an existing composer.lock —
    // there's nothing to pin without one (matches Composer, which refuses
    // a partial update with no lock present).
    let partial = if packages.is_empty() {
        None
    } else {
        let lock_path = project_root.join("composer.lock");
        if !lock_path.is_file() {
            return Err(eyre::eyre!(
                "cannot update a partial set of packages without a composer.lock present — \
                 run `bougie composer update` (no package list) first to create one",
            ));
        }
        let lock = Lock::read(&lock_path)
            .wrap_err_with(|| format!("reading {}", lock_path.display()))?;
        // Root requirements drive `-w`'s "leave root requires pinned"
        // rule. Read them straight from composer.json (`require` +
        // `require-dev` keys); only needed for the `-w` path, but cheap
        // to collect unconditionally.
        let root_requires = read_root_require_names(&project_root);
        let partial = PartialUpdate {
            names: packages,
            with_dependencies,
            with_all_dependencies,
            root_requires,
            lock,
        };
        for name in partial.unknown_names() {
            eprintln!(
                "warning: {name} is not present in composer.lock — \
                 it will be added if another requirement pulls it in",
            );
        }
        Some(partial)
    };

    if dry_run {
        // Preserve the lighter dry-run path: single solve, just names
        // + versions. Useful when the user wants a quick "what would
        // change" without paying for the second prod-only solve.
        let summary: UpdateSummary = match &partial {
            Some(p) => dry_run_update_partial(
                &paths,
                &project_root,
                Repo::packagist(),
                DryRunOptions { no_dev, resolution },
                Some(p),
                &ignore_platform,
            )?,
            None => dry_run_update(
                &paths,
                &project_root,
                Repo::packagist(),
                DryRunOptions { no_dev, resolution },
                &ignore_platform,
            )?,
        };
        let result = UpdateResult {
            schema_version: 1,
            project_root,
            no_dev: summary.no_dev,
            dry_run: true,
            packages: summary.packages,
            packages_dev: Vec::new(),
            lock_path: None,
            packages_installed: None,
            packages_already_present: None,
        };
        emit(format, &result)?;
        return Ok(ExitCode::SUCCESS);
    }

    // Write-mode path: resolve, build a Lock, atomic write.
    let (lock_path, outcome) = resolve_and_write_lock_partial(
        &paths,
        &project_root,
        partial.as_ref(),
        resolution,
        &ignore_platform,
    )?;

    // Then materialize vendor/ — Composer's `update` resolves *and*
    // installs. `--no-install` stops after the lock.
    let install = if no_install {
        None
    } else {
        let project = bougie_config::load_project(&project_root)?;
        let patch_plan = super::patches::build_plan(&paths, &project_root, &project, None)?;
        Some(
            bougie_composer_resolver::install_from_lock_with_patches(
                &paths,
                &project_root,
                InstallOptions { no_dev },
                None,
                patch_plan.as_ref(),
            )
            .wrap_err("installing dependencies from the updated lock")?,
        )
    };

    let result = UpdateResult {
        schema_version: 1,
        project_root,
        no_dev,
        dry_run: false,
        packages: outcome
            .packages
            .iter()
            .map(|p| ResolvedPackage {
                name: p.name.clone(),
                version: p.version.clone(),
            })
            .collect(),
        packages_dev: outcome
            .packages_dev
            .iter()
            .map(|p| ResolvedPackage {
                name: p.name.clone(),
                version: p.version.clone(),
            })
            .collect(),
        lock_path: Some(lock_path),
        packages_installed: install.as_ref().map(|s| s.packages_installed),
        packages_already_present: install.as_ref().map(|s| s.packages_already_present),
    };
    // Telemetry enrichment (TELEMETRY.md): bucketed totals + counts +
    // perf, never names.
    bougie_telemetry::probe::record(|p| {
        p.enrich.packages_installed = result.packages_installed;
        p.enrich.total_deps = Some(bougie_telemetry::probe::bucket(
            result.packages.len() + result.packages_dev.len(),
        ));
        if let Some(s) = &install {
            p.enrich.download_bytes = Some(s.download_bytes);
            p.enrich.autoload_ms = Some(s.autoload_ms);
            p.enrich.cache_hit_pct = bougie_telemetry::probe::cache_hit_pct(
                u64::from(s.packages_already_present),
                u64::from(s.packages_installed),
            );
        }
    });
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Resolve `composer.json` against Packagist + any configured
/// repositories, build a `Lock`, and atomically write it to
/// `<project_root>/composer.lock`. Returns the lock path and the
/// resolver outcome (so the caller can render output without
/// re-reading the file).
///
/// Shared by `bougie composer update` (the headline verb) and the
/// `bougie composer install` fallback path that triggers when no
/// `composer.lock` exists yet — mirrors what Composer itself does
/// in `Composer\Installer::run` (see the
/// `'No composer.lock file present. Updating dependencies to
/// latest instead of installing from lock file'` warning).
pub fn resolve_and_write_lock(
    paths: &Paths,
    project_root: &Path,
    resolution: ResolutionStrategy,
) -> Result<(PathBuf, LockfileSolveOutcome)> {
    resolve_and_write_lock_partial(
        paths,
        project_root,
        None,
        resolution,
        &PlatformIgnore::default(),
    )
}

/// Like [`resolve_and_write_lock`], but with an optional [`PartialUpdate`]
/// so `composer update <pkg>...` pins out-of-scope packages (`None` is a
/// full update) and an `ignore_platform` filter so the resolve-time half
/// of `--ignore-platform-req(s)` drops the matching platform edges.
pub fn resolve_and_write_lock_partial(
    paths: &Paths,
    project_root: &Path,
    partial: Option<&PartialUpdate>,
    resolution: ResolutionStrategy,
    ignore_platform: &PlatformIgnore,
) -> Result<(PathBuf, LockfileSolveOutcome)> {
    let (composer_json_bytes, outcome): (Vec<u8>, LockfileSolveOutcome) =
        resolve_for_lockfile_partial(
            paths,
            project_root,
            Repo::packagist(),
            partial,
            resolution,
            ignore_platform,
        )?;
    let lock_path = project_root.join("composer.lock");

    let t_hash = std::time::Instant::now();
    let content_hash = lockfile::content_hash(&composer_json_bytes)
        .wrap_err("computing composer.json content-hash")?;
    tracing::info!(
        elapsed_ms = u64::try_from(t_hash.elapsed().as_millis()).unwrap_or(u64::MAX),
        composer_json_bytes = composer_json_bytes.len(),
        "content_hash",
    );

    let lock = Lock {
        readme: canonical_readme(),
        content_hash: Some(content_hash),
        packages: outcome.packages.clone(),
        packages_dev: outcome.packages_dev.clone(),
        aliases: Vec::new(),
        minimum_stability: Some(outcome.minimum_stability.clone()),
        stability_flags: outcome.stability_flags.clone(),
        prefer_stable: outcome.prefer_stable,
        // Composer records `--prefer-lowest` in the lock; mirror that for
        // any non-default resolution policy (`lowest` / `lowest-direct`).
        prefer_lowest: resolution != ResolutionStrategy::Highest,
        platform: BTreeMap::new(),
        platform_dev: BTreeMap::new(),
        platform_overrides: BTreeMap::new(),
        plugin_api_version: Some("2.6.0".into()),
    };

    let t_write = std::time::Instant::now();
    lockfile::write_lock(&lock_path, &lock)
        .wrap_err_with(|| format!("writing {}", lock_path.display()))?;
    tracing::info!(
        elapsed_ms = u64::try_from(t_write.elapsed().as_millis()).unwrap_or(u64::MAX),
        packages = outcome.packages.len(),
        packages_dev = outcome.packages_dev.len(),
        lock_path = %lock_path.display(),
        "write_lock",
    );
    Ok((lock_path, outcome))
}

/// Collect the project's root requirement names — the keys of
/// `composer.json`'s `require` and `require-dev` objects. Feeds
/// [`PartialUpdate::root_requires`] so `-w` knows which transitive deps
/// to leave pinned. A missing or malformed file yields an empty list
/// (the resolver re-parses and reports composer.json errors itself).
fn read_root_require_names(project_root: &Path) -> Vec<String> {
    let path = project_root.join("composer.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for key in ["require", "require-dev"] {
        if let Some(obj) = json.get(key).and_then(serde_json::Value::as_object) {
            names.extend(obj.keys().cloned());
        }
    }
    names
}

