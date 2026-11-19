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
    DryRunOptions, PartialUpdate, PlatformIgnore, ResolutionStrategy, dry_run_update_partial,
};
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use composer_semver::constraint::Constraint;
use composer_semver::version::Version;
use eyre::{Context, Result, eyre};
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<ToolchainLockResult>,
}

/// What `--with-platform` / `--all-targets` / `--upgrade` asked for.
#[derive(Debug, Clone)]
pub struct PlatformLock {
    pub enabled: bool,
    pub all_targets: bool,
    pub upgrade: super::platform_lock::Upgrade,
}

#[derive(Debug, Serialize)]
pub struct ToolchainLockResult {
    pub dry_run: bool,
    pub php_version: String,
    pub extensions: usize,
    pub services: usize,
    /// Targets the lock carries digests for.
    pub targets: Vec<String>,
    /// Per-entry changes against the previous `bougie.lock` (empty when
    /// there was none, or nothing moved).
    pub changes: Vec<String>,
    /// Targets `--all-targets` could not cover, with the reason.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_targets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_path: Option<PathBuf>,
}

impl Render for LockResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.already_in_sync {
            writeln!(w, "composer.lock is already in sync with composer.json")?;
        } else if let Some(p) = &self.lock_path {
            let count = self
                .packages
                .map_or_else(|| "packages".to_string(), |n| format!("{n} packages"));
            if !self.changed.is_empty() {
                writeln!(w, "re-resolving: {}", self.changed.join(", "))?;
            }
            writeln!(w, "wrote {} ({count})", p.display())?;
        } else if self.dry_run {
            let count = self
                .packages
                .map_or_else(|| "packages".to_string(), |n| format!("{n} packages"));
            if !self.changed.is_empty() {
                writeln!(w, "re-resolving: {}", self.changed.join(", "))?;
            }
            writeln!(w, "lock --dry-run: would write composer.lock ({count})")?;
        }
        if let Some(toolchain) = &self.toolchain {
            let summary = format!(
                "PHP {}, {} extensions, {} services",
                toolchain.php_version, toolchain.extensions, toolchain.services
            );
            for change in &toolchain.changes {
                writeln!(w, "  {change}")?;
            }
            for skipped in &toolchain.skipped_targets {
                writeln!(w, "  skipped {skipped}")?;
            }
            if toolchain.dry_run {
                writeln!(w, "lock --dry-run: would write bougie.lock ({summary})")?;
            } else if let Some(path) = &toolchain.lock_path {
                writeln!(w, "wrote {} ({summary})", path.display())?;
            }
        }
        Ok(())
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
    with_platform: PlatformLock,
    resolution: ResolutionStrategy,
    ignore_platform: PlatformIgnore,
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
        if dry_run {
            let summary = dry_run_update_partial(
                &paths,
                &project_root,
                Repo::packagist(),
                DryRunOptions {
                    no_dev: false,
                    resolution,
                },
                None,
                &ignore_platform,
            )?;
            return finish_with_platform(
                format,
                LockResult {
                    schema_version: 1,
                    project_root,
                    already_in_sync: false,
                    dry_run: true,
                    changed: Vec::new(),
                    packages: Some(summary.packages.len()),
                    lock_path: None,
                    toolchain: None,
                },
                with_platform,
                &paths,
            );
        }
        let (path, outcome) = super::composer_update::resolve_and_write_lock_partial(
            &paths,
            &project_root,
            None,
            resolution,
            &ignore_platform,
        )?;
        return finish_with_platform(
            format,
            LockResult {
                schema_version: 1,
                project_root,
                already_in_sync: false,
                dry_run: false,
                changed: Vec::new(),
                packages: Some(outcome.packages.len() + outcome.packages_dev.len()),
                lock_path: Some(path),
                toolchain: None,
            },
            with_platform,
            &paths,
        );
    }

    let lock =
        Lock::read(&lock_path).wrap_err_with(|| format!("reading {}", lock_path.display()))?;

    // (2) Content-hash matches → already consistent; nothing to do.
    let content_hash = lockfile::content_hash(&composer_json_bytes)
        .wrap_err("computing composer.json content-hash")?;
    if lock.content_hash.as_deref() == Some(content_hash.as_str()) {
        return finish_with_platform(
            format,
            LockResult {
                schema_version: 1,
                project_root,
                already_in_sync: true,
                dry_run,
                changed: Vec::new(),
                packages: Some(lock.all_packages().count()),
                lock_path: None,
                toolchain: None,
            },
            with_platform,
            &paths,
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
            DryRunOptions {
                no_dev: false,
                resolution,
            },
            Some(&partial),
            &ignore_platform,
        )?;
        return finish_with_platform(
            format,
            LockResult {
                schema_version: 1,
                project_root,
                already_in_sync: false,
                dry_run: true,
                changed,
                packages: Some(summary.packages.len()),
                lock_path: None,
                toolchain: None,
            },
            with_platform,
            &paths,
        );
    }

    let (path, outcome) = super::composer_update::resolve_and_write_lock_partial(
        &paths,
        &project_root,
        Some(&partial),
        resolution,
        &ignore_platform,
    )?;
    finish_with_platform(
        format,
        LockResult {
            schema_version: 1,
            project_root,
            already_in_sync: false,
            dry_run: false,
            changed,
            packages: Some(outcome.packages.len() + outcome.packages_dev.len()),
            lock_path: Some(path),
            toolchain: None,
        },
        with_platform,
        &paths,
    )
}

/// Root requires (`require` + `require-dev`) that must be re-resolved:
/// those the lock does not satisfy, or whose constraint no longer matches
/// the locked version. Platform packages (php, ext-*, …) are skipped — they
/// aren't lock packages. A constraint that fails to parse is treated as
/// changed so the resolver surfaces the error.
///
/// A require is also satisfied by a locked package that `replace`s or
/// `provide`s it with a compatible constraint: the resolver picks the
/// replacer, so the replaced name never gets a lock entry of its own
/// (`buckaroo/magento2` replaces `buckaroo/magento2secondchance`, and
/// Mage-OS ships many such replacements). Without this, every such require
/// was reported as re-resolved on every run.
fn changed_requires(composer_json: &serde_json::Value, lock: &Lock) -> Vec<String> {
    use std::collections::HashMap;
    let mut locked: HashMap<String, Version> = HashMap::new();
    // name → (link constraint, locked version of the package declaring it)
    let mut stand_ins: HashMap<String, Vec<(&str, Option<Version>)>> = HashMap::new();
    for p in lock.all_packages() {
        let raw = p.version_normalized.as_deref().unwrap_or(&p.version);
        let version = Version::parse(raw).ok();
        if let Some(v) = &version {
            locked.insert(p.name.to_ascii_lowercase(), v.clone());
        }
        for (target, link) in p.replace.iter().chain(&p.provide) {
            stand_ins
                .entry(target.to_ascii_lowercase())
                .or_default()
                .push((link.as_str(), version.clone()));
        }
    }
    let mut changed = Vec::new();
    for key in ["require", "require-dev"] {
        let Some(obj) = composer_json
            .get(key)
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        for (name, constraint) in obj {
            if is_platform(name) {
                continue;
            }
            let Some(constraint) = constraint.as_str() else {
                continue;
            };
            let Ok(wanted) = Constraint::parse(constraint) else {
                changed.push(name.clone());
                continue;
            };
            let lower = name.to_ascii_lowercase();
            let by_package = locked.get(&lower).is_some_and(|v| wanted.matches(v));
            let by_stand_in = || {
                stand_ins.get(&lower).is_some_and(|links| {
                    links
                        .iter()
                        .any(|(link, owner)| link_satisfies(link, owner.as_ref(), &wanted))
                })
            };
            if !by_package && !by_stand_in() {
                changed.push(name.clone());
            }
        }
    }
    changed
}

/// Whether a `replace`/`provide` link constraint can satisfy `wanted`:
/// `self.version` stands for the declaring package's own locked version;
/// anything else must intersect the requirement, as Composer checks it.
fn link_satisfies(link: &str, owner: Option<&Version>, wanted: &Constraint) -> bool {
    if link.trim() == "self.version" {
        return owner.is_some_and(|v| wanted.matches(v));
    }
    Constraint::parse(link).is_ok_and(|c| c.intersects(wanted))
}

fn read_root_require_names(composer_json: &serde_json::Value) -> Vec<String> {
    let mut names = Vec::new();
    for key in ["require", "require-dev"] {
        if let Some(obj) = composer_json
            .get(key)
            .and_then(serde_json::Value::as_object)
        {
            names.extend(obj.keys().cloned());
        }
    }
    names
}

fn finish(format: OutputFormat, result: &LockResult) -> Result<ExitCode> {
    // Telemetry enrichment (TELEMETRY.md): the lock's package total,
    // bucketed — no names.
    bougie_telemetry::probe::record(|p| {
        if let Some(n) = result.packages {
            p.enrich.total_deps = Some(bougie_telemetry::probe::bucket(n));
        }
    });
    emit(format, result)?;
    Ok(ExitCode::SUCCESS)
}

fn finish_with_platform(
    format: OutputFormat,
    mut result: LockResult,
    with_platform: PlatformLock,
    paths: &Paths,
) -> Result<ExitCode> {
    if with_platform.enabled {
        result.toolchain = Some(lock_toolchain(
            &result.project_root,
            paths,
            with_platform.all_targets,
            &with_platform.upgrade,
            result.dry_run,
        )?);
    }
    finish(format, &result)
}

/// Resolve and (unless `dry_run`) write `bougie.lock`. Re-locking keeps
/// existing pins unless `upgrade` releases them, and keeps other targets'
/// digests when the pins did not move; `all_targets` fills in every
/// target the index serves.
fn lock_toolchain(
    project_root: &std::path::Path,
    paths: &Paths,
    all_targets: bool,
    upgrade: &super::platform_lock::Upgrade,
    dry_run: bool,
) -> Result<ToolchainLockResult> {
    use super::platform_lock;

    let previous = bougie_lock::ToolchainLock::read(project_root)?;
    let ctx = platform_lock::IndexCtx::open(paths)?;
    let prefer = platform_lock::Preferences::new(previous.as_ref(), upgrade);
    let mut lock = platform_lock::resolve(project_root, &ctx, prefer)?;
    for name in upgrade.unknown_names(&lock) {
        eprintln!("warning: --upgrade-package {name} matches nothing in bougie.lock");
    }
    if let Some(previous) = &previous {
        platform_lock::merge_previous(&mut lock, previous);
    }
    let mut skipped_targets = Vec::new();
    if all_targets {
        let targets: Vec<String> = ctx.root.targets.keys().cloned().collect();
        for target in targets {
            if lock.targets.contains_key(&target) {
                continue;
            }
            // A target that does not publish a locked version cannot use
            // this lock at all; say so rather than failing the whole lock.
            match platform_lock::resolve_target(&ctx, &lock, &target) {
                Ok(artifacts) => {
                    lock.targets.insert(target, artifacts);
                }
                Err(error) => skipped_targets.push(format!("{target}: {error}")),
            }
        }
    }
    let changes = platform_lock::describe_changes(previous.as_ref(), &lock);
    let lock_path = if dry_run {
        None
    } else {
        Some(lock.write(project_root)?)
    };
    Ok(ToolchainLockResult {
        dry_run,
        php_version: lock.php.version.clone(),
        extensions: lock.extensions.len(),
        services: lock.services.len(),
        targets: lock.targets.keys().cloned().collect(),
        changes,
        skipped_targets,
        lock_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lock_of(packages: serde_json::Value) -> Lock {
        serde_json::from_value(json!({
            "content-hash": "x",
            "packages": packages,
            "packages-dev": [],
        }))
        .unwrap()
    }

    #[test]
    fn replaced_require_is_not_changed() {
        // The large Mage-OS project: four requires are satisfied by
        // replacers and have no lock entry of their own. They were listed
        // under "re-resolving:" on every `bougie lock`.
        let lock = lock_of(json!([
            {"name": "buckaroo/magento2", "version": "v2.7.6",
             "version_normalized": "2.7.6.0",
             "replace": {"buckaroo/magento2secondchance": "*"}},
            {"name": "mage-os/module-admin-activity-log", "version": "2.0.4",
             "version_normalized": "2.0.4.0",
             "replace": {"kiwicommerce/module-admin-activity": "1.0.8"}},
            {"name": "acme/polyfill", "version": "1.2.0",
             "version_normalized": "1.2.0.0",
             "provide": {"acme/iface": "self.version"}},
        ]));
        let composer_json = json!({"require": {
            "buckaroo/magento2": ">=2.7",
            "buckaroo/magento2secondchance": ">=1.1",
            "kiwicommerce/module-admin-activity": ">=1.0",
            "acme/iface": "^1.0",
        }});
        assert!(changed_requires(&composer_json, &lock).is_empty());
    }

    #[test]
    fn incompatible_replace_or_missing_package_is_changed() {
        let lock = lock_of(json!([
            {"name": "mage-os/module-admin-activity-log", "version": "2.0.4",
             "version_normalized": "2.0.4.0",
             "replace": {"kiwicommerce/module-admin-activity": "1.0.8"}},
            {"name": "acme/polyfill", "version": "1.2.0",
             "version_normalized": "1.2.0.0",
             "provide": {"acme/iface": "self.version"}},
            {"name": "acme/foo", "version": "1.0.0", "version_normalized": "1.0.0.0"},
        ]));
        let composer_json = json!({"require": {
            "kiwicommerce/module-admin-activity": "^2.0",
            "acme/iface": "^2.0",
            "acme/foo": "^2.0",
            "acme/new": "^1.0",
            "php": ">=8.1",
        }});
        let mut changed = changed_requires(&composer_json, &lock);
        changed.sort();
        assert_eq!(
            changed,
            vec![
                "acme/foo".to_string(),
                "acme/iface".to_string(),
                "acme/new".to_string(),
                "kiwicommerce/module-admin-activity".to_string(),
            ]
        );
    }
}
