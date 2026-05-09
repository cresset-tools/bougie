use crate::cli::OutputFormat;
use crate::config::load_project;
use crate::output::{emit, Render};
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub schema_version: u32,
    pub required: Vec<String>,
}

impl Render for ListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.required.is_empty() {
            writeln!(w, "no extensions required by composer.json")?;
            return Ok(());
        }
        for ext in &self.required {
            writeln!(w, "required  ext-{ext}")?;
        }
        Ok(())
    }
}

/// Phase 8 minimum: list the `ext-*` keys from `composer.json`'s
/// `require` block. Cross-referencing against the index ("available"
/// rows) lands once the cached section is plumbed through here.
pub fn run(format: OutputFormat, field: Option<&str>) -> Result<ExitCode> {
    let project_root = std::env::current_dir()?;
    let project = load_project(&project_root)?;
    let mut required: Vec<String> = project
        .composer
        .map(|c| c.require_extensions.into_iter().collect())
        .unwrap_or_default();
    required.sort();
    let result = ListResult { schema_version: 1, required };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}
