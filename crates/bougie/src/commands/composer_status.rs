//! `bougie composer status` — report packages that look locally
//! modified or locally sourced.
//!
//! Composer's `status` exists to catch the case where you've edited
//! files inside a `vendor/` package installed from source (it runs `git
//! status` per source install). bougie installs from dist archives,
//! which carry no source-tracking, so for the overwhelming common case
//! there is nothing to report. What it *can* surface honestly is
//! packages installed from a local `path` repository — those point at
//! working directories you may be editing — so it lists those.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer::lockfile::Lock;
use bougie_output::output::{emit, Render};
use eyre::{eyre, Context, Result};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct StatusResult {
    pub schema_version: u32,
    /// Packages installed from a local `path` dist (working-directory
    /// references that may be edited in place).
    pub path_packages: Vec<String>,
}

impl Render for StatusResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.path_packages.is_empty() {
            return writeln!(w, "No local changes");
        }
        writeln!(
            w,
            "The following packages are installed from a local path and may have local changes:"
        )?;
        for p in &self.path_packages {
            writeln!(w, "  {p}")?;
        }
        Ok(())
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "wired from clap-parsed CLI; ownership crosses the boundary"
)]
pub fn run(format: OutputFormat, working_dir: Option<PathBuf>) -> Result<ExitCode> {
    let project_root = match working_dir {
        Some(p) => p,
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };
    let lock_path = project_root.join("composer.lock");
    if !lock_path.is_file() {
        return Err(eyre!(
            "no composer.lock in {} — run `bougie composer install` or `update` first",
            project_root.display()
        ));
    }
    let lock = Lock::read(&lock_path).wrap_err_with(|| format!("reading {}", lock_path.display()))?;

    let mut path_packages: Vec<String> = lock
        .all_packages()
        .filter(|p| p.is_path_dist())
        .map(|p| p.name.clone())
        .collect();
    path_packages.sort();

    emit(format, &StatusResult { schema_version: 1, path_packages })?;
    Ok(ExitCode::SUCCESS)
}
