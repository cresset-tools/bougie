//! `bougie services remove <name>… [--purge]`. CLI.md §3.8.2.
//!
//! Without `--purge` this removes the config entry only and leaves any
//! provisioned tenant data in place (re-adding restores it). With
//! `--purge` it first runs the same deprovision-and-destroy path as
//! `bougie services down --purge` for the named services, then removes
//! their declarations.

use super::client;
use super::config_mut::{choose_config_target, locate_project_root, remove_service, ConfigTarget};
use bougie_cli::OutputFormat;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{self, Write};
use std::process::ExitCode;

/// Subset of the daemon's `service.down` reply we surface back.
#[derive(Debug, Deserialize)]
struct DownReply {
    #[serde(default)]
    deprovisioned: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ServicesRemoveResult {
    pub schema_version: u32,
    pub items: Vec<RemoveItem>,
    pub target: String,
    /// True when `--purge` was passed and the deprovision path ran.
    pub purged: bool,
    /// Services whose tenant data was destroyed by the purge (empty when
    /// `--purge` wasn't given or nothing was provisioned).
    pub deprovisioned: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct RemoveItem {
    pub name: String,
    /// True if an entry was actually removed; false if it wasn't present.
    pub removed: bool,
}

impl Render for ServicesRemoveResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for s in &self.deprovisioned {
            writeln!(w, "destroyed tenant data for {s}")?;
        }
        for it in &self.items {
            if it.removed {
                writeln!(w, "removed {} from {}", it.name, self.target)?;
            } else {
                writeln!(w, "not declared: {}", it.name)?;
            }
        }
        Ok(())
    }
}

#[allow(clippy::needless_pass_by_value, reason = "owned strings from clap-parsed CLI")]
pub fn run(
    format: OutputFormat,
        names: Vec<String>,
    purge: bool,
) -> Result<ExitCode> {
    if names.is_empty() {
        return Err(eyre!("no services specified"));
    }
    let project_root = locate_project_root()?;
    let target = choose_config_target(&project_root)?;
    let target_label = match &target {
        ConfigTarget::Composer(p) => p.display().to_string(),
        ConfigTarget::Toml(p) => p.display().to_string(),
    };

    // `--purge` destroys tenant data first (same daemon path as `bougie
    // services down --purge`), *then* we drop the declarations. Order
    // matters: deprovisioning is tenant-scoped off the still-present
    // config, so it has to run before `remove_service`.
    let deprovisioned = if purge {
        let args = json!({
            "project": project_root,
            "services": names,
            "purge": true,
        });
        let paths = Paths::from_env()?;
        let reply: DownReply = client::call(&paths, "service.down", args)?;
        reply.deprovisioned
    } else {
        Vec::new()
    };

    let mut items = Vec::with_capacity(names.len());
    for name in &names {
        let removed = remove_service(&target, name)?;
        items.push(RemoveItem { name: name.clone(), removed });
    }

    let result = ServicesRemoveResult {
        schema_version: 1,
        items,
        target: target_label,
        purged: purge,
        deprovisioned,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
