//! `bougie service list [--all]`. CLI.md §3.8.3.
//!
//! Phase 2 lists the services declared in the project's config.
//! `--all` is parsed but degrades to project-scoped output until
//! Phase 3 wires the cross-project tenant query against `bougied`.

use super::config_mut::locate_project_root;
use bougie_cli::OutputFormat;
use bougie_config::{load_project, ServicePin};
use bougie_output::output::{Render, emit};
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ServicesListResult {
    pub schema_version: u32,
    pub project: String,
    pub services: Vec<ServiceRow>,
}

#[derive(Debug, Serialize)]
pub struct ServiceRow {
    pub name: String,
    pub version: String,
    pub tenant: Option<String>,
}

impl Render for ServicesListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.services.is_empty() {
            writeln!(w, "no services declared in {}", self.project)?;
            return Ok(());
        }
        for s in &self.services {
            match &s.tenant {
                Some(t) => writeln!(w, "{:14} {:10} tenant={t}", s.name, s.version)?,
                None => writeln!(w, "{:14} {}", s.name, s.version)?,
            }
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, _all: bool) -> Result<ExitCode> {
    let project_root = locate_project_root()?;
    let project = load_project(&project_root)?;

    let mut rows: Vec<ServiceRow> = project
        .bougie
        .services
        .iter()
        .map(|(name, pin)| ServiceRow {
            name: name.clone(),
            version: pin_version(pin).to_string(),
            tenant: pin.tenant().map(str::to_owned),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));

    let result = ServicesListResult {
        schema_version: 1,
        project: project_root.display().to_string(),
        services: rows,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn pin_version(pin: &ServicePin) -> &str {
    pin.version().unwrap_or("*")
}
