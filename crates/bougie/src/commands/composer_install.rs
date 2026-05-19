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

pub fn run(
    format: OutputFormat,
    working_dir: Option<PathBuf>,
    no_dev: bool,
    _frozen: bool,
) -> Result<ExitCode> {
    let project_root = match working_dir {
        Some(p) => p,
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };
    let paths = Paths::from_env()?;
    let summary = install_from_lock(&paths, &project_root, InstallOptions { no_dev })?;
    emit(format, &InstallResult::from(summary))?;
    Ok(ExitCode::SUCCESS)
}
