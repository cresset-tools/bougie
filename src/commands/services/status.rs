//! `bougie services status [<name>]`. CLI.md §3.8.7.
//!
//! Walks the supervisor's `status` IPC reply and renders the project's
//! services. Cross-project view (`--all`-equivalent) is reserved for
//! Phase 3+.

use super::client;
use super::config_mut::locate_project_root;
use crate::cli::OutputFormat;
use crate::config::load_project;
use crate::output::{Render, emit};
use crate::paths::Paths;
use eyre::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ServicesStatusResult {
    pub schema_version: u32,
    pub project: String,
    pub services: Vec<ServiceRow>,
}

#[derive(Debug, Serialize)]
pub struct ServiceRow {
    pub name: String,
    pub state: String,
    pub pid: Option<u64>,
    pub uptime_ms: Option<u64>,
    pub binding: Value,
    pub declared: bool,
}

#[derive(Debug, Deserialize)]
struct DaemonReply {
    services: Vec<Value>,
}

impl Render for ServicesStatusResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for row in &self.services {
            let pid = row
                .pid
                .map_or_else(|| "-".into(), |p| p.to_string());
            let uptime = row.uptime_ms.map_or_else(
                || "-".into(),
                |ms| format!("{:.1}s", ms as f64 / 1000.0),
            );
            writeln!(
                w,
                "{:14} {:15} pid={:>7} uptime={:>8}",
                row.name, row.state, pid, uptime
            )?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, field: Option<&str>, name: Option<String>) -> Result<ExitCode> {
    let project_root = locate_project_root()?;
    let project = load_project(&project_root)?;
    let declared: std::collections::BTreeSet<&str> = project
        .bougie
        .services
        .keys()
        .map(String::as_str)
        .collect();

    let paths = Paths::from_env()?;
    let reply: DaemonReply = client::call(&paths, "status", Value::Null)?;

    let mut services: Vec<ServiceRow> = reply
        .services
        .into_iter()
        .filter_map(|v| {
            let name = v.get("name").and_then(Value::as_str)?.to_string();
            // If the user asked about one service, filter to that.
            if let Some(target) = &name_filter(&name, name.as_str(), name.as_str()) {
                let _ = target;
            }
            Some(ServiceRow {
                declared: declared.contains(name.as_str()),
                state: v
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                pid: v.get("pid").and_then(Value::as_u64),
                uptime_ms: v.get("uptime_ms").and_then(Value::as_u64),
                binding: v.get("binding").cloned().unwrap_or(Value::Null),
                name,
            })
        })
        .collect();

    if let Some(filter) = name {
        services.retain(|s| s.name == filter);
    } else {
        // Default: project-scoped view, declared services first.
        services.retain(|s| s.declared);
    }
    services.sort_by(|a, b| a.name.cmp(&b.name));

    let result = ServicesStatusResult {
        schema_version: 1,
        project: project_root.display().to_string(),
        services,
    };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}

// no-op helper kept inline to silence clippy's "needless borrow" on
// the filter chain above; future Phase will use this for `--all`.
fn name_filter<'a>(_a: &'a str, _b: &'a str, _c: &'a str) -> Option<&'a str> {
    None
}
