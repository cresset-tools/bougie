//! `bougie tool uninstall <vendor/name>`.

use bougie_cli::OutputFormat;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use bougie_tool::{request, uninstall};
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ToolUninstallResult {
    pub schema_version: u32,
    pub package: String,
    pub tool_dir: PathBuf,
    pub removed_bins: Vec<PathBuf>,
}

impl Render for ToolUninstallResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "uninstalled {} ({} bin(s) removed)",
            self.package,
            self.removed_bins.len()
        )
    }
}

pub fn run(format: OutputFormat, package: &str) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let req = request::parse(package)?;
    let outcome = uninstall::uninstall(&paths, &req.package())?;
    emit(
        format,
        &ToolUninstallResult {
            schema_version: 1,
            package: outcome.package,
            tool_dir: outcome.tool_dir,
            removed_bins: outcome.removed_bins,
        },
    )?;
    Ok(ExitCode::SUCCESS)
}
