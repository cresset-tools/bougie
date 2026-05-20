//! `bougie composer install` — project install. Reads `composer.json`
//! + `composer.lock` in the working directory, verifies the
//! content-hash, parallel-downloads dists into `vendor/`, and emits
//! `vendor/autoload.php` + `vendor/composer/installed.{json,php}`.
//!
//! The binary-management surface this verb used to expose
//! (`bougie composer install <version>`) lives at
//! `bougie composer fetch <version>` now — see `composer_fetch.rs`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer_resolver::verify::{verify_lock, VerifyOptions, VerifyOutcome};
use bougie_composer_resolver::{install_from_lock, InstallOptions, InstallSummary};
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use eyre::{Context, Result};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub schema_version: u32,
    pub project_root: PathBuf,
    pub packages_installed: u32,
    pub packages_already_present: u32,
    pub no_dev: bool,
}

impl From<InstallSummary> for InstallResult {
    fn from(s: InstallSummary) -> Self {
        Self {
            schema_version: 1,
            project_root: s.project_root,
            packages_installed: s.packages_installed,
            packages_already_present: s.packages_already_present,
            no_dev: s.no_dev,
        }
    }
}

impl Render for InstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let total = self.packages_installed + self.packages_already_present;
        let mode = if self.no_dev { " (no-dev)" } else { "" };
        writeln!(
            w,
            "installed {total} packages ({} fresh, {} cached){mode} → {}",
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

    let summary = install_from_lock(&paths, &project_root, InstallOptions { no_dev })?;
    emit(format, &InstallResult::from(summary))?;
    Ok(ExitCode::SUCCESS)
}
