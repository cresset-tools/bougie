use bougie_cli::OutputFormat;
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct CleanResult {
    pub schema_version: u32,
    pub removed: PathBuf,
}

impl Render for CleanResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "wiped {}", self.removed.display())
    }
}

pub fn run(format: OutputFormat) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    if paths.cache().exists() {
        std::fs::remove_dir_all(paths.cache())
            .wrap_err_with(|| format!("removing {}", paths.cache().display()))?;
    }
    let result = CleanResult { schema_version: 1, removed: paths.cache().to_path_buf() };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
