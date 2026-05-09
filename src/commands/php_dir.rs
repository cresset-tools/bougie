use crate::cli::OutputFormat;
use crate::output::{emit, Render};
use crate::paths::Paths;
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct PhpDirResult {
    pub schema_version: u32,
    pub path: PathBuf,
}

impl Render for PhpDirResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}", self.path.display())
    }
}

pub fn run(format: OutputFormat, field: Option<&str>) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let result = PhpDirResult {
        schema_version: 1,
        path: paths.installs(),
    };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}
