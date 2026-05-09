use crate::cli::OutputFormat;
use crate::output::{emit, Render};
use crate::paths::Paths;
use crate::store::list_installed;
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub schema_version: u32,
    pub installed: Vec<InstalledRow>,
}

#[derive(Debug, Serialize)]
pub struct InstalledRow {
    pub version: String,
    pub flavor: String,
}

impl Render for ListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.installed.is_empty() {
            writeln!(w, "no PHP interpreters installed")?;
            return Ok(());
        }
        for row in &self.installed {
            writeln!(w, "installed  {} ({})", row.version, row.flavor)?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, field: Option<&str>) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let installed = list_installed(&paths)?
        .into_iter()
        .map(|(version, flavor)| InstalledRow { version, flavor })
        .collect();
    let result = ListResult { schema_version: 1, installed };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}
