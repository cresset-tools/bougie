//! `bougie composer update` — resolve `composer.json` from scratch
//! and report what the new `composer.lock` would contain.
//!
//! Currently ships **dry-run only**: the lockfile writer is a
//! follow-up. Running the verb without `--dry-run` errors out with a
//! pointer to the issue tracking the write-path.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use bougie_cli::OutputFormat;
use bougie_composer_resolver::metadata::base_url;
use bougie_composer_resolver::{dry_run_update, DryRunOptions, ResolvedPackage, UpdateSummary};
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use eyre::{eyre, Context, Result};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct UpdateResult {
    pub schema_version: u32,
    pub project_root: PathBuf,
    pub no_dev: bool,
    pub dry_run: bool,
    pub packages: Vec<ResolvedPackage>,
}

impl Render for UpdateResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.packages.is_empty() {
            writeln!(
                w,
                "composer update --dry-run: no packages to install (composer.json has no external requires)",
            )?;
            return Ok(());
        }
        let mode = if self.no_dev { " (no-dev)" } else { "" };
        writeln!(
            w,
            "composer update --dry-run{mode}: {} packages",
            self.packages.len(),
        )?;
        for p in &self.packages {
            writeln!(w, "  {} {}", p.name, p.version)?;
        }
        writeln!(w)?;
        writeln!(
            w,
            "(read-only preview — `composer.lock` is not written until the lockfile writer lands)",
        )?;
        Ok(())
    }
}

pub fn run(
    format: OutputFormat,
    working_dir: Option<PathBuf>,
    no_dev: bool,
    dry_run: bool,
) -> Result<ExitCode> {
    if !dry_run {
        return Err(eyre!(
            "writing `composer.lock` isn't implemented yet — pass `--dry-run` to preview \
             the resolution. The lockfile writer is the next slice of Phase C.",
        ));
    }

    let project_root = match working_dir {
        Some(p) => p,
        None => std::env::current_dir().wrap_err("reading current directory")?,
    };
    let paths = Paths::from_env()?;
    let summary: UpdateSummary =
        dry_run_update(&paths, &project_root, &base_url(), DryRunOptions { no_dev })?;

    let result = UpdateResult {
        schema_version: 1,
        project_root,
        no_dev: summary.no_dev,
        dry_run: true,
        packages: summary.packages,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
