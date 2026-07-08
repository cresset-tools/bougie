//! `bougie service add <name>[@<version>]…`. CLI.md §3.8.1.

use super::config_mut::{add_service, choose_config_target, locate_project_root, ConfigTarget};
use bougie_cli::OutputFormat;
use bougie_daemon::daemon::catalog;
use bougie_output::output::{Render, emit};
use eyre::{eyre, Result};
use serde::Serialize;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ServicesAddResult {
    pub schema_version: u32,
    pub items: Vec<AddItem>,
    pub target: String,
}

#[derive(Debug, Serialize)]
pub struct AddItem {
    pub name: String,
    pub version: String,
    /// True if the entry was already present at the same pin.
    pub already_present: bool,
}

impl Render for ServicesAddResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        for it in &self.items {
            if it.already_present {
                writeln!(w, "already declared: {} = {:?}", it.name, it.version)?;
            } else {
                writeln!(w, "added {} = {:?} to {}", it.name, it.version, self.target)?;
            }
        }
        Ok(())
    }
}

#[allow(clippy::needless_pass_by_value, reason = "owned strings from clap-parsed CLI")]
pub fn run(format: OutputFormat, names: Vec<String>) -> Result<ExitCode> {
    if names.is_empty() {
        return Err(eyre!("no services specified"));
    }
    let parsed: Vec<(String, Option<String>)> = names
        .iter()
        .map(|raw| parse_name_with_optional_version(raw))
        .collect::<Result<_>>()?;

    // Validate every name against the user-facing catalog up front; we
    // want a hard error before any write so a typo doesn't leave the
    // project half-edited.
    for (name, _) in &parsed {
        match catalog::find(name) {
            Some(e) if e.user_facing => {}
            Some(_) => {
                return Err(eyre!(
                    "`{name}` is a runtime dep and cannot be added directly; known: {}",
                    catalog::user_facing_names()
                ));
            }
            None => {
                return Err(eyre!(
                    "unknown service `{name}` (known: {})",
                    catalog::user_facing_names()
                ));
            }
        }
    }

    let project_root = locate_project_root()?;

    // A project runs one relational DB, not both. Reject a mariadb/mysql
    // clash across the union of what's already declared and what this
    // command adds — before any write, so the project never lands in the
    // conflicting state. (Also catches `service add mariadb mysql` in a
    // single invocation.)
    let existing = bougie_config::load_project(&project_root)
        .map(|p| p.bougie.services.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let declared: Vec<&str> = existing
        .iter()
        .map(String::as_str)
        .chain(parsed.iter().map(|(n, _)| n.as_str()))
        .collect();
    if let Some((a, b)) = catalog::exclusive_conflict(declared) {
        return Err(eyre!(
            "a project can run only one of `{a}` and `{b}` — they're mutually \
             exclusive relational databases. Keep one; `bougie service remove {other}` \
             the other first.",
            other = if parsed.iter().any(|(n, _)| n == a) { b } else { a },
        ));
    }

    let target = choose_config_target(&project_root)?;
    let target_label = match &target {
        ConfigTarget::Composer(p) => p.display().to_string(),
        ConfigTarget::Toml(p) => p.display().to_string(),
    };

    let mut items = Vec::with_capacity(parsed.len());
    for (name, version) in parsed {
        let pin = version.unwrap_or_else(|| "*".into());
        let was_new = add_service(&target, &name, &pin)?;
        items.push(AddItem {
            name,
            version: pin,
            already_present: !was_new,
        });
    }

    let result = ServicesAddResult { schema_version: 1, items, target: target_label };
    emit(format, &result)?;

    // Materialize the new services' client-tool shims (mysqldump,
    // redis-cli, …) in vendor/bougie/bin/ right away rather than waiting
    // for the next sync. Best-effort — the declaration edit above is
    // the command's contract.
    crate::commands::sync::refresh_service_client_shims(&project_root);
    Ok(ExitCode::SUCCESS)
}

/// `redis` or `redis@8.6` → `(name, version?)`. Mirrors the `ext add`
/// parser; @-version is the only constraint shape we accept here.
fn parse_name_with_optional_version(raw: &str) -> Result<(String, Option<String>)> {
    if let Some((name, ver)) = raw.split_once('@') {
        if name.is_empty() {
            return Err(eyre!("service name cannot be empty: {raw:?}"));
        }
        if ver.is_empty() {
            return Err(eyre!("service version cannot be empty: {raw:?}"));
        }
        Ok((name.to_string(), Some(ver.to_string())))
    } else {
        Ok((raw.to_string(), None))
    }
}
