//! `bougie down [<name>…] [--purge]` — promoted from the former
//! `bougie services down`. Handler path is unchanged; only the
//! user-facing CLI verb moved. See CLI.md §3.8.5.

use super::client;
use super::config_mut::locate_project_root;
use crate::cli::OutputFormat;
use crate::config::load_project;
use crate::output::{Render, emit};
use crate::paths::Paths;
use eyre::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ServicesDownResult {
    pub schema_version: u32,
    pub stopped: Vec<String>,
    pub deprovisioned: Vec<String>,
    pub purged: bool,
}

#[derive(Debug, Deserialize)]
struct DaemonReply {
    #[serde(default)]
    stopped: Vec<String>,
    #[serde(default)]
    deprovisioned: Vec<String>,
}

impl Render for ServicesDownResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for s in &self.deprovisioned {
            writeln!(w, "deprovisioned tenant for {s}")?;
        }
        for s in &self.stopped {
            writeln!(w, "stopped {s}")?;
        }
        if self.deprovisioned.is_empty() && self.stopped.is_empty() {
            writeln!(w, "nothing to do (no matching tenants)")?;
        }
        if self.purged {
            writeln!(w, "(purge mode: tenant data destroyed)")?;
        }
        Ok(())
    }
}

pub fn run(
    format: OutputFormat,
        names: Vec<String>,
    purge: bool,
) -> Result<ExitCode> {
    let project_root = locate_project_root()?;
    let project = load_project(&project_root)?;

    let services: Vec<String> = if names.is_empty() {
        project.bougie.services.keys().cloned().collect()
    } else {
        names
    };

    let args = json!({
        "project": project_root,
        "services": services,
        "purge": purge,
    });

    let paths = Paths::from_env()?;
    let reply: DaemonReply = client::call(&paths, "service.down", args)?;
    let result = ServicesDownResult {
        schema_version: 1,
        stopped: reply.stopped,
        deprovisioned: reply.deprovisioned,
        purged: purge,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
