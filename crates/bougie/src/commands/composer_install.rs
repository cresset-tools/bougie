//! `bougie composer install` — project install. Reads `composer.json`
//! + `composer.lock` in the working directory, verifies the
//! content-hash, parallel-downloads dists into `vendor/`, and emits
//! `vendor/autoload.php` + `vendor/composer/installed.{json,php}`.
//!
//! Composer phar version management (the verbs `bougie composer` used to
//! expose: `fetch`, `list`, `pin`, …) was removed; the version is pinned
//! via `bougie.toml [composer] version` and fetched lazily during
//! `bougie sync`. Any non-native composer subcommand is forwarded to the
//! real Composer phar via `shim::run_project_composer`.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer::lockfile::Lock;
use bougie_composer_resolver::verify::{verify_lock, VerifyOptions, VerifyOutcome};
use bougie_composer_resolver::{install_from_lock, InstallOptions, InstallSummary};
use bougie_installer::baseline;
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use bougie_semver::constraint::Constraint;
use bougie_semver::version::Version;
use eyre::{eyre, Context, Result};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub schema_version: u32,
    pub project_root: PathBuf,
    pub packages_installed: u32,
    pub packages_already_present: u32,
    pub packages_up_to_date: u32,
    pub packages_skipped_plugin: u32,
    pub packages_removed: u32,
    pub bins_installed: u32,
    /// Files copied into the project root by the native Magento deploy
    /// (`extra.map`). Zero for non-Magento projects.
    pub files_deployed: u64,
    pub no_dev: bool,
    pub warnings: Vec<String>,
}

impl From<InstallSummary> for InstallResult {
    fn from(s: InstallSummary) -> Self {
        Self {
            schema_version: 2,
            project_root: s.project_root,
            packages_installed: s.packages_installed,
            packages_already_present: s.packages_already_present,
            packages_up_to_date: s.packages_up_to_date,
            packages_skipped_plugin: s.packages_skipped_plugin,
            packages_removed: s.packages_removed,
            bins_installed: s.bins_installed,
            files_deployed: s.files_deployed,
            no_dev: s.no_dev,
            warnings: s.warnings,
        }
    }
}

impl Render for InstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        // Warnings to stderr so they survive `--format json-v1`
        // pipelines that capture stdout. Matches the
        // missing-composer.lock warning emitted earlier in run().
        for warning in &self.warnings {
            eprintln!("warning: {warning}");
        }
        let total = self.packages_installed
            + self.packages_already_present
            + self.packages_up_to_date;
        let mode = if self.no_dev { " (no-dev)" } else { "" };

        // When everything is up-to-date, emit a short "nothing to install" line.
        if self.packages_installed == 0
            && self.packages_already_present == 0
            && self.packages_up_to_date > 0
        {
            let removed = if self.packages_removed > 0 {
                format!(", {} removed", self.packages_removed)
            } else {
                String::new()
            };
            return writeln!(
                w,
                "{total} packages up to date{removed}{mode} → {}",
                self.project_root.display(),
            );
        }

        let up_to_date = if self.packages_up_to_date > 0 {
            format!(", {} up to date", self.packages_up_to_date)
        } else {
            String::new()
        };
        let skipped = if self.packages_skipped_plugin > 0 {
            format!(", {} plugin(s) skipped", self.packages_skipped_plugin)
        } else {
            String::new()
        };
        let removed = if self.packages_removed > 0 {
            format!(", {} removed", self.packages_removed)
        } else {
            String::new()
        };
        let bins = if self.bins_installed > 0 {
            format!(", {} bin(s)", self.bins_installed)
        } else {
            String::new()
        };
        let deployed = if self.files_deployed > 0 {
            format!(", {} file(s) deployed", self.files_deployed)
        } else {
            String::new()
        };
        writeln!(
            w,
            "installed {total} packages ({} fresh, {} cached{up_to_date}{skipped}{removed}{bins}{deployed}){mode} → {}",
            self.packages_installed,
            self.packages_already_present,
            self.project_root.display(),
        )
    }
}

#[derive(Debug, Serialize)]
pub struct LockVerifyResult {
    pub schema_version: u32,
    pub project_root: PathBuf,
    pub valid: bool,
    /// When `valid` is false, the derivation tree rendered by
    /// pubgrub's `DefaultStringReporter`. Empty when valid.
    pub reason: Option<String>,
}

impl Render for LockVerifyResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.valid {
            writeln!(w, "composer.lock is valid → {}", self.project_root.display())
        } else {
            writeln!(w, "composer.lock is INVALID")?;
            if let Some(r) = &self.reason {
                writeln!(w)?;
                writeln!(w, "{r}")?;
            }
            Ok(())
        }
    }
}

pub fn run(
    format: OutputFormat,
    working_dir: Option<PathBuf>,
    no_dev: bool,
    _frozen: bool,
    lock_verify: bool,
    ignore_platform_reqs: bool,
    ignore_platform_req: Vec<String>,
) -> Result<ExitCode> {
    let project_root = match working_dir {
        Some(p) => p,
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };
    if lock_verify {
        let outcome = verify_lock(&project_root, VerifyOptions { no_dev })?;
        let (valid, reason) = match outcome {
            VerifyOutcome::Valid => (true, None),
            VerifyOutcome::Invalid { reason } => (false, Some(reason)),
        };
        let result = LockVerifyResult {
            schema_version: 1,
            project_root,
            valid,
            reason,
        };
        emit(format, &result)?;
        return Ok(if valid { ExitCode::SUCCESS } else { ExitCode::FAILURE });
    }
    let paths = Paths::from_env()?;

    // Composer-compatible fallback: when composer.lock is absent,
    // resolve from composer.json + write the lock first, then run
    // the normal install-from-lock path. Mirrors
    // `Composer\Installer::run` —
    //   "No composer.lock file present. Updating dependencies to
    //    latest instead of installing from lock file."
    // (See https://getcomposer.org/install.) The warning goes to
    // stderr so JSON-output consumers on stdout aren't affected.
    let lock_path = project_root.join("composer.lock");
    if !lock_path.is_file() {
        eprintln!(
            "warning: composer.lock not found; resolving dependencies from composer.json. \
             A fresh composer.lock will be written.",
        );
        super::composer_update::resolve_and_write_lock(&paths, &project_root)?;
    }

    if !ignore_platform_reqs {
        check_platform_requirements(
            &project_root,
            no_dev,
            &ignore_platform_req,
        )?;
    }

    let summary = install_from_lock(&paths, &project_root, InstallOptions { no_dev })?;
    emit(format, &InstallResult::from(summary))?;
    Ok(ExitCode::SUCCESS)
}

fn check_platform_requirements(
    project_root: &Path,
    no_dev: bool,
    ignore_specific: &[String],
) -> Result<()> {
    let lock_path = project_root.join("composer.lock");
    if !lock_path.exists() {
        return Ok(());
    }
    let lock = Lock::read(&lock_path)?;

    let php_version = match detect_php_version(project_root) {
        Some(v) => v,
        None => return Ok(()),
    };
    let available_extensions = detect_available_extensions(project_root);

    let ignored: BTreeSet<&str> = ignore_specific.iter().map(String::as_str).collect();

    let packages = if no_dev {
        lock.packages.iter().collect::<Vec<_>>()
    } else {
        lock.all_packages().collect::<Vec<_>>()
    };

    let mut errors: Vec<String> = Vec::new();

    for pkg in &packages {
        for (dep_name, raw_constraint) in &pkg.require {
            if dep_name == "php" {
                if ignored.contains("php") {
                    continue;
                }
                let Ok(constraint) = Constraint::parse(raw_constraint) else {
                    continue;
                };
                if !constraint.matches(&php_version) {
                    errors.push(format!(
                        "{} requires php {} but {} is installed",
                        pkg.name, raw_constraint, php_version,
                    ));
                }
            } else if let Some(ext_name) = dep_name.strip_prefix("ext-") {
                if ignored.contains(dep_name.as_str()) {
                    continue;
                }
                if !available_extensions.contains(ext_name) {
                    errors.push(format!(
                        "{} requires {} but it is not installed. \
                         Install it with: bougie ext add {ext_name}",
                        pkg.name, dep_name,
                    ));
                }
            }
        }
    }

    // Also check root composer.json requires
    let composer_json_path = project_root.join("composer.json");
    if let Ok(bytes) = std::fs::read(&composer_json_path) {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            for key in if no_dev { &["require"][..] } else { &["require", "require-dev"] } {
                let Some(reqs) = value.get(key).and_then(|v| v.as_object()) else {
                    continue;
                };
                for (dep_name, raw) in reqs {
                    let Some(raw_str) = raw.as_str() else { continue };
                    if dep_name == "php" {
                        if ignored.contains("php") {
                            continue;
                        }
                        let Ok(constraint) = Constraint::parse(raw_str) else {
                            continue;
                        };
                        if !constraint.matches(&php_version) {
                            errors.push(format!(
                                "root composer.json requires php {raw_str} but {php_version} is installed",
                            ));
                        }
                    } else if let Some(ext_name) = dep_name.strip_prefix("ext-") {
                        if ignored.contains(dep_name.as_str()) {
                            continue;
                        }
                        if !available_extensions.contains(ext_name) {
                            errors.push(format!(
                                "root composer.json requires {dep_name} but it is not installed. \
                                 Install it with: bougie ext add {ext_name}",
                            ));
                        }
                    }
                }
            }
        }
    }

    if errors.is_empty() {
        return Ok(());
    }

    errors.dedup();
    let bullets = errors
        .iter()
        .map(|e| format!("  - {e}"))
        .collect::<Vec<_>>()
        .join("\n");
    Err(eyre!(
        "your platform does not satisfy the requirements of the installed packages:\n\
         {bullets}\n\n\
         Pass --ignore-platform-reqs to skip this check.",
    ))
}

fn detect_php_version(project_root: &Path) -> Option<Version> {
    // Try bougie's resolved PHP first
    if let Ok((version_str, _flavor)) = bougie_fs::state::read_project_resolved(project_root) {
        if let Ok(v) = Version::parse(&version_str) {
            return Some(v);
        }
    }
    // Fall back to system PHP
    let output = std::process::Command::new("php")
        .arg("-r")
        .arg("echo PHP_MAJOR_VERSION.'.'.PHP_MINOR_VERSION.'.'.PHP_RELEASE_VERSION;")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version_str = String::from_utf8_lossy(&output.stdout);
    Version::parse(version_str.trim()).ok()
}

fn detect_available_extensions(project_root: &Path) -> BTreeSet<String> {
    let mut exts: BTreeSet<String> = BTreeSet::new();

    // Builtin extensions (statically compiled into PHP)
    for ext in baseline::BUILTIN_EXTENSIONS {
        exts.insert((*ext).to_string());
    }

    // Baseline extensions (installed by default)
    for ext in baseline::BASELINE_EXTENSIONS {
        exts.insert((*ext).to_string());
    }

    // Project conf.d fragments
    let confd = project_root.join(".bougie").join("conf.d");
    if let Ok(entries) = std::fs::read_dir(&confd) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Fragment names are like "20-redis.ini" or "35-pdo_mysql.ini",
            // and may carry multiple numeric prefixes ("00-20-gd.ini").
            // PHP extension names never contain '-', so the name is
            // whatever follows the *last* '-' — split from the right so
            // multi-segment prefixes don't leak into the name.
            if let Some(ext_name) = name.strip_suffix(".ini") {
                if let Some((_prefix, ext)) = ext_name.rsplit_once('-') {
                    exts.insert(ext.to_string());
                }
            }
        }
    }

    // Also try system PHP if no bougie state
    if !project_root.join(".bougie").join("state").join("resolved").exists() {
        if let Ok(output) = std::process::Command::new("php").arg("-m").output() {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let mut in_extensions = false;
                for line in stdout.lines() {
                    let trimmed = line.trim();
                    if trimmed == "[PHP Modules]" {
                        in_extensions = true;
                        continue;
                    }
                    if trimmed == "[Zend Modules]" {
                        break;
                    }
                    if in_extensions && !trimmed.is_empty() {
                        exts.insert(trimmed.to_lowercase());
                    }
                }
            }
        }
    }

    exts
}
