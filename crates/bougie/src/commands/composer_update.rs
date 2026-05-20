//! `bougie composer update` — resolve `composer.json` from scratch
//! and either preview (`--dry-run`) or write a fresh `composer.lock`.
//!
//! Default mode writes the lock atomically; `--dry-run` skips the
//! write and prints what would land. `vendor/` is still not touched
//! here — that's `composer install`'s job. Matches Composer's
//! split: `update` builds the lock, `install` materializes it.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer::lockfile::{self, canonical_readme, Lock};
use bougie_composer_resolver::metadata::Repo;
use bougie_composer_resolver::{
    dry_run_update, resolve_for_lockfile, DryRunOptions, LockfileSolveOutcome, ResolvedPackage,
    UpdateSummary,
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
        }
        Ok(())
    }
}

pub fn run(
    format: OutputFormat,
    working_dir: Option<PathBuf>,
    no_dev: bool,
    dry_run: bool,
) -> Result<ExitCode> {
    let project_root = match working_dir {
        Some(p) => p,
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };
    let paths = Paths::from_env()?;

    if dry_run {
        // Preserve the lighter dry-run path: single solve, just names
        // + versions. Useful when the user wants a quick "what would
        // change" without paying for the second prod-only solve.
        let summary: UpdateSummary =
            dry_run_update(&paths, &project_root, Repo::packagist(), DryRunOptions { no_dev })?;
        let result = UpdateResult {
            schema_version: 1,
            project_root,
            no_dev: summary.no_dev,
            dry_run: true,
            packages: summary.packages,
            packages_dev: Vec::new(),
            lock_path: None,
        };
        emit(format, &result)?;
        return Ok(ExitCode::SUCCESS);
    }

    // Write-mode path: resolve, build a Lock, atomic write.
    let (lock_path, outcome) = resolve_and_write_lock(&paths, &project_root)?;

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
    };
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
) -> Result<(PathBuf, LockfileSolveOutcome)> {
    let (composer_json_bytes, outcome): (Vec<u8>, LockfileSolveOutcome) =
        resolve_for_lockfile(paths, project_root, Repo::packagist())?;
    let lock_path = project_root.join("composer.lock");

    let content_hash = lockfile::content_hash(&composer_json_bytes)
        .wrap_err("computing composer.json content-hash")?;

    let lock = Lock {
        readme: canonical_readme(),
        content_hash: Some(content_hash),
        packages: outcome.packages.clone(),
        packages_dev: outcome.packages_dev.clone(),
        aliases: Vec::new(),
        minimum_stability: Some(outcome.minimum_stability.clone()),
        stability_flags: outcome.stability_flags.clone(),
        prefer_stable: outcome.prefer_stable,
        prefer_lowest: false,
        platform: BTreeMap::new(),
        platform_dev: BTreeMap::new(),
        platform_overrides: BTreeMap::new(),
        plugin_api_version: Some("2.6.0".into()),
    };

    lockfile::write_lock(&lock_path, &lock)
        .wrap_err_with(|| format!("writing {}", lock_path.display()))?;
    Ok((lock_path, outcome))
}

