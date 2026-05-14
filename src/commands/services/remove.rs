//! `bougie services remove <name>… [--purge]`. CLI.md §3.8.2.
//!
//! Phase 2 removes the config entry only. The `--purge` flag is parsed
//! but treated as "remove from config and that's all" until the
//! provisioner deprovisioning path lands in Phase 3 (data-destruction
//! is risky and should not exist in dry form).

use super::config_mut::{choose_config_target, locate_project_root, remove_service, ConfigTarget};
use crate::cli::OutputFormat;
use crate::output::{Render, emit};
use eyre::{eyre, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ServicesRemoveResult {
    pub schema_version: u32,
    pub items: Vec<RemoveItem>,
    pub target: String,
    /// Echoed back so future tooling can detect that purge was
    /// requested even though Phase 2 doesn't act on it.
    pub purge_requested: bool,
}

#[derive(Debug, Serialize)]
pub struct RemoveItem {
    pub name: String,
    /// True if an entry was actually removed; false if it wasn't present.
    pub removed: bool,
}

impl Render for ServicesRemoveResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for it in &self.items {
            if it.removed {
                writeln!(w, "removed {} from {}", it.name, self.target)?;
            } else {
                writeln!(w, "not declared: {}", it.name)?;
            }
        }
        if self.purge_requested {
            writeln!(
                w,
                "note: --purge has no effect in this release — tenant data \
                 stays until the supervisor (Phase 3+) wires deprovisioning"
            )?;
        }
        Ok(())
    }
}

#[allow(clippy::needless_pass_by_value, reason = "owned strings from clap-parsed CLI")]
pub fn run(
    format: OutputFormat,
    field: Option<&str>,
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

    let mut items = Vec::with_capacity(names.len());
    for name in names {
        let removed = remove_service(&target, &name)?;
        items.push(RemoveItem { name, removed });
    }

    let result = ServicesRemoveResult {
        schema_version: 1,
        items,
        target: target_label,
        purge_requested: purge,
    };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}
