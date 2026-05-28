//! `bougie tool install <vendor/name>[@<constraint>] [--force]`.
//!
//! Bridges `bougie-tool::install::install` (which is composer-agnostic
//! by design) to the bougie binary's existing composer-resolve helper
//! at `composer_update::resolve_and_write_lock`. Keeping the bougie
//! crate's resolver glue out of `bougie-tool` avoids a circular crate
//! dep — `bougie-composer-resolver` already lives below us in the
//! workspace graph for the install step; only the *lock generation*
//! glue lives up here.

use bougie_cli::OutputFormat;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use bougie_tool::{install, request};
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ToolInstallResult {
    pub schema_version: u32,
    pub package: String,
    pub php_version: String,
    pub tool_dir: PathBuf,
    pub installed_bins: Vec<PathBuf>,
}

impl Render for ToolInstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        let bins = self
            .installed_bins
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            w,
            "installed {} (php {}) → {bins}",
            self.package, self.php_version
        )
    }
}

pub fn run(format: OutputFormat, package: &str, force: bool) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let req = request::parse(package)?;
    let resolve_lock: &install::LockResolver = &|paths, project_root| {
        super::composer_update::resolve_and_write_lock(paths, project_root)
            .map(|_| ())
    };
    let outcome = install::install(&paths, &req, force, resolve_lock)?;
    emit(
        format,
        &ToolInstallResult {
            schema_version: 1,
            package: outcome.package,
            php_version: outcome.php_version,
            tool_dir: outcome.tool_dir,
            installed_bins: outcome.installed_bins,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}
