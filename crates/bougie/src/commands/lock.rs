//! `bougie lock` — minimal lockfile refresh (uv's `uv lock`).
//!
//! Reconciles `composer.lock` with `composer.json` while holding every
//! package at its currently-locked version where that's still valid. It
//! never bumps versions and never installs into `vendor/`. To pull newer
//! versions, use `bougie composer update`.
//!
//! Flow:
//! 1. No `composer.lock` → a full resolve writes a fresh one.
//! 2. `composer.json`'s content-hash matches the lock → already in sync,
//!    no-op (offline, no resolve).
//! 3. Otherwise re-resolve **only** the root requires that are new or
//!    whose constraint no longer matches the locked version (`changed`),
//!    holding everything else pinned via `PartialUpdate`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer::lockfile::{self, Lock};
use bougie_composer_resolver::metadata::Repo;
use bougie_composer_resolver::verify::is_platform;
use bougie_composer_resolver::{
    dry_run_update_partial, DryRunOptions, PartialUpdate, ResolutionStrategy,
};
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use composer_semver::constraint::Constraint;
use composer_semver::version::Version;
use eyre::{eyre, Context, Result};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct LockResult {
    pub schema_version: u32,
    pub project_root: PathBuf,
    /// `true` when `composer.json`'s content-hash already matched the
    /// lock — nothing was re-resolved or written.
    pub already_in_sync: bool,
    pub dry_run: bool,
    /// Root requires that were re-resolved (new or re-constrained).
    pub changed: Vec<String>,
    /// Total packages in the resulting lock (when known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packages: Option<usize>,
    /// Path written (omitted for dry-run / no-op).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_path: Option<PathBuf>,
}

impl Render for LockResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.already_in_sync {
            return writeln!(w, "composer.lock is already in sync with composer.json");
        }
        let count = self
            .packages
            .map_or_else(|| "packages".to_string(), |n| format!("{n} packages"));
        if !self.changed.is_empty() {
            writeln!(w, "re-resolving: {}", self.changed.join(", "))?;
        }
        if self.dry_run {
            writeln!(w, "lock --dry-run: would write composer.lock ({count})")
        } else if let Some(p) = &self.lock_path {
            writeln!(w, "wrote {} ({count})", p.display())
        } else {
            Ok(())
        }
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "wired from clap-parsed CLI; ownership crosses the boundary"
)]
pub fn run(
    format: OutputFormat,
    working_dir: Option<PathBuf>,
    dry_run: bool,
    resolution: ResolutionStrategy,
) -> Result<ExitCode> {
    let project_root = match working_dir {
        Some(p) => p,
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };
    let paths = Paths::from_env()?;

    let composer_json_path = project_root.join("composer.json");
    let composer_json_bytes = std::fs::read(&composer_json_path).map_err(|e| {
        eyre!(
            "{} not found — not a Composer project: {e}",
            composer_json_path.display()
        )
    })?;
    let composer_json: serde_json::Value = serde_json::from_slice(&composer_json_bytes)
        .map_err(|e| eyre!("parsing composer.json: {e}"))?;

    let lock_path = project_root.join("composer.lock");

    // (1) No lock yet → full resolve writes a fresh one.
    if !lock_path.is_file() {
        let (path, outcome) =
            super::composer_update::resolve_and_write_lock(&paths, &project_root, resolution)?;
        return finish(
            format,
            &LockResult {
                schema_version: 1,
                project_root,
                already_in_sync: false,
                dry_run: false,
                changed: Vec::new(),
                packages: Some(outcome.packages.len() + outcome.packages_dev.len()),
                lock_path: Some(path),
            },
        );
    }

    let lock = Lock::read(&lock_path).wrap_err_with(|| format!("reading {}", lock_path.display()))?;

    // (2) Content-hash matches → already consistent; nothing to do.
    let content_hash = lockfile::content_hash(&composer_json_bytes)
        .wrap_err("computing composer.json content-hash")?;
    if lock.content_hash.as_deref() == Some(content_hash.as_str()) {
        return finish(
            format,
            &LockResult {
                schema_version: 1,
                project_root,
                already_in_sync: true,
                dry_run,
                changed: Vec::new(),
                packages: Some(lock.all_packages().count()),
                lock_path: None,
            },
        );
    }

    // (3) Re-resolve only the changed root requires; pin the rest.
    let changed = changed_requires(&composer_json, &lock);
    let root_requires = read_root_require_names(&composer_json);
    let partial = PartialUpdate {
        names: changed.clone(),
        // Let a changed package's own subtree move if its new constraint
        // requires it; everything else stays pinned. Minimal, but allows
        // necessary movement.
        with_dependencies: true,
        with_all_dependencies: false,
        root_requires,
        lock,
    };

    if dry_run {
        let summary = dry_run_update_partial(
            &paths,
            &project_root,
            Repo::packagist(),
            DryRunOptions { no_dev: false, resolution },
            Some(&partial),
        )?;
        return finish(
            format,
            &LockResult {
                schema_version: 1,
                project_root,
                already_in_sync: false,
                dry_run: true,
                changed,
                packages: Some(summary.packages.len()),
                lock_path: None,
            },
        );
    }

    let (path, outcome) = super::composer_update::resolve_and_write_lock_partial(
        &paths,
        &project_root,
        Some(&partial),
        resolution,
    )?;
    finish(
        format,
        &LockResult {
            schema_version: 1,
            project_root,
            already_in_sync: false,
            dry_run: false,
            changed,
            packages: Some(outcome.packages.len() + outcome.packages_dev.len()),
            lock_path: Some(path),
        },
    )
}

/// Root requires (`require` + `require-dev`) that must be re-resolved:
/// those absent from the lock, or whose constraint no longer matches the
/// locked version. Platform packages (php, ext-*, …) are skipped — they
/// aren't lock packages. A constraint that fails to parse is treated as
/// changed so the resolver surfaces the error.
fn changed_requires(composer_json: &serde_json::Value, lock: &Lock) -> Vec<String> {
    let mut locked: std::collections::HashMap<String, Version> = std::collections::HashMap::new();
    for p in lock.all_packages() {
        let raw = p.version_normalized.as_deref().unwrap_or(&p.version);
        if let Ok(v) = Version::parse(raw) {
            locked.insert(p.name.to_ascii_lowercase(), v);
        }
    }
    let mut changed = Vec::new();
    for key in ["require", "require-dev"] {
        let Some(obj) = composer_json.get(key).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, constraint) in obj {
            if is_platform(name) {
                continue;
            }
            let Some(constraint) = constraint.as_str() else {
                continue;
            };
            let satisfied = locked
                .get(&name.to_ascii_lowercase())
                .is_some_and(|v| Constraint::parse(constraint).is_ok_and(|c| c.matches(v)));
            if !satisfied {
                changed.push(name.clone());
            }
        }
    }
    changed
}

fn read_root_require_names(composer_json: &serde_json::Value) -> Vec<String> {
    let mut names = Vec::new();
    for key in ["require", "require-dev"] {
        if let Some(obj) = composer_json.get(key).and_then(serde_json::Value::as_object) {
            names.extend(obj.keys().cloned());
        }
    }
    names
}

fn finish(format: OutputFormat, result: &LockResult) -> Result<ExitCode> {
    emit(format, result)?;
    Ok(ExitCode::SUCCESS)
}
