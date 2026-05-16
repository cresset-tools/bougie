use crate::cli::OutputFormat;
use crate::output::{emit, Render};
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct PruneResult {
    pub schema_version: u32,
    pub message: &'static str,
}

impl Render for PruneResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}", self.message)
    }
}

/// Phase 8 stub: the reachability walk over the store needs to read
/// every project's enabled extensions through their cached manifests.
/// That ride-along arrives once `bougie ext` has a concrete enabled
/// set per project. Until then, prune is a no-op so users don't
/// accidentally remove store paths bougie still needs.
pub fn run(format: OutputFormat, _dry_run: bool) -> Result<ExitCode> {
    let result = PruneResult {
        schema_version: 1,
        message:
            "cache prune: nothing to do (reachability walk lands once `bougie ext` tracks per-project enabled extensions)",
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
