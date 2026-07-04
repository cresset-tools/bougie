//! `bougie service restart [<name>…]`. SERVICES.md §7.2.
//!
//! Restarts the underlying global service process. The tenant ledger
//! is **not** touched: generated passwords + DB numbers survive, so
//! apps that have them cached keep working after the restart. This
//! is therefore "kick the broker" semantics, not "re-provision."
//!
//! Affects every project sharing the same service. There is no
//! per-project restart story in v1 — services are global, so a
//! restart is global by construction.

use super::client;
use super::config_mut::locate_project_root;
use bougie_cli::OutputFormat;
use bougie_config::{load_project, ServicePin};
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ServicesRestartResult {
    pub schema_version: u32,
    /// Names of services that were actually restarted (i.e. were
    /// running or starting before the call). Skipped if the service
    /// was already Stopped.
    pub restarted: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DaemonReply {
    #[serde(default)]
    restarted: Vec<String>,
}

impl Render for ServicesRestartResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.restarted.is_empty() {
            writeln!(w, "no services to restart")?;
            return Ok(());
        }
        for s in &self.restarted {
            writeln!(w, "restarted {s}")?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, names: Vec<String>) -> Result<ExitCode> {
    let project_root = locate_project_root()?;
    let project = load_project(&project_root)?;

    // Same selection logic as `up`: with no argument, restart every
    // declared service. With names, intersect against declarations
    // and error if the user names something they haven't declared.
    let declared: Vec<(String, &ServicePin)> = project
        .bougie
        .services
        .iter()
        .map(|(k, v)| (k.clone(), v))
        .collect();
    let selected: Vec<String> = if names.is_empty() {
        declared.into_iter().map(|(k, _)| k).collect()
    } else {
        let mut out = Vec::new();
        for n in &names {
            if declared.iter().any(|(k, _)| k == n) {
                out.push(n.clone());
            } else {
                return Err(eyre!(
                    "service `{n}` isn't declared in this project (try `bougie service add {n}` first)"
                ));
            }
        }
        out
    };
    if selected.is_empty() {
        emit(format, &ServicesRestartResult {
            schema_version: 1,
            restarted: vec![],
        })?;
        return Ok(ExitCode::SUCCESS);
    }

    let args = json!({
        "project": project_root,
        "services": selected,
    });
    let paths = Paths::from_env()?;
    let reply: DaemonReply = client::call(&paths, "service.restart", args)?;
    let result = ServicesRestartResult {
        schema_version: 1,
        restarted: reply.restarted,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
