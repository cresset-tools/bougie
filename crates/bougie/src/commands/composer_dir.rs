use bougie_cli::OutputFormat;
use bougie_output::output::{emit, Render};
use bougie_paths::Paths;
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ComposerDirResult {
    pub schema_version: u32,
    pub path: PathBuf,
}

impl Render for ComposerDirResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}", self.path.display())
    }
}

pub fn run(format: OutputFormat) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let result = ComposerDirResult {
        schema_version: 1,
        path: paths.composer_root(),
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
