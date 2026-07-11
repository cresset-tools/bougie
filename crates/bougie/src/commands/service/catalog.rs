//! `bougie service catalog` — print the built-in catalog. No daemon
//! involvement. See SERVICES.md §2.

use bougie_cli::OutputFormat;
use bougie_daemon::daemon::catalog::{self, Binding, CatalogEntry, Tenancy};
use bougie_output::output::{Render, emit};
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct CatalogResult {
    pub schema_version: u32,
    pub entries: Vec<CatalogRow>,
}

#[derive(Debug, Serialize)]
pub struct CatalogRow {
    pub name: String,
    /// The catalog *default* version — what a bare `bougie up <name>` runs.
    pub version: String,
    /// Every version the catalog can run for this service (default first).
    /// A single-version service lists just `[version]`; a multi-version one
    /// like `mysql` lists both (8.4 + 8.0), which are the values a
    /// `[services]` pin can resolve to.
    pub versions: Vec<String>,
    pub binding: Binding,
    pub tenancy: Tenancy,
    pub user_facing: bool,
    pub summary: String,
}

impl From<&CatalogEntry> for CatalogRow {
    fn from(e: &CatalogEntry) -> Self {
        Self {
            name: e.name.into(),
            version: e.version.into(),
            versions: e.versions.iter().map(|v| (*v).to_string()).collect(),
            binding: e.binding,
            tenancy: e.tenancy,
            user_facing: e.user_facing,
            summary: e.summary.into(),
        }
    }
}

impl Render for CatalogResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for row in &self.entries {
            if !row.user_facing {
                continue;
            }
            let binding = match row.binding {
                Binding::UnixSocket { sockname } => format!("socket ({sockname})"),
                Binding::Tcp { port } => format!("tcp 127.0.0.1:{port}"),
                Binding::None => "—".to_string(),
            };
            writeln!(
                w,
                "{name:14} {vers:20} {bind:24} {summary}",
                name = row.name,
                vers = row.versions.join(", "),
                bind = binding,
                summary = row.summary,
            )?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat) -> Result<ExitCode> {
    let entries: Vec<CatalogRow> = catalog::CATALOG.iter().map(CatalogRow::from).collect();
    let result = CatalogResult { schema_version: 1, entries };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
