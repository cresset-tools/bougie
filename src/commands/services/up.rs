//! `bougie up [<name>…]` — promoted to a top-level verb from its
//! original home as `bougie services up`. The module path keeps the
//! `services::up` name because the handler still belongs to the
//! services subsystem semantically; only the user-facing CLI surface
//! moved. See CLI.md §3.8.4.

use super::client;
use super::config_mut::locate_project_root;
use crate::cli::OutputFormat;
use crate::config::{load_project, ServicePin};
use crate::daemon::store_fetch::ResolvedTool;
use crate::output::{Render, emit};
use crate::paths::Paths;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ServicesUpResult {
    pub schema_version: u32,
    pub started: Vec<String>,
    pub tenants: BTreeMap<String, String>,
    /// Per-service inventory of resolved tool dependencies. Populated
    /// for services whose auto-fetch path walked a non-empty
    /// `requires_tools[]`; empty (or absent at the JSON layer when
    /// serialized via `skip_serializing_if`) for services that were
    /// already on disk or have no inner-tool deps.
    ///
    /// Per `UNBUNDLE_PLAN.md` Phase 4. Schema bumped to 2 because the
    /// envelope shape grew this field; other CLI command results stay
    /// at `schema_version=1`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, Vec<ResolvedTool>>,
}

#[derive(Debug, Deserialize)]
struct DaemonReply {
    #[serde(default)]
    started: Vec<String>,
    #[serde(default)]
    tenants: BTreeMap<String, String>,
    #[serde(default)]
    dependencies: BTreeMap<String, Vec<ResolvedTool>>,
}

impl Render for ServicesUpResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.started.is_empty() && self.tenants.is_empty() {
            writeln!(w, "no services to start")?;
            return Ok(());
        }
        for s in &self.started {
            writeln!(w, "started {s}")?;
        }
        for (svc, tenant) in &self.tenants {
            writeln!(w, "tenant for {svc}: {tenant}")?;
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, names: Vec<String>) -> Result<ExitCode> {
    let project_root = locate_project_root()?;
    let project = load_project(&project_root)?;

    // Figure out which services to bring up. With no argument, every
    // service declared in the project. With names, intersection of the
    // request with the project's declarations.
    let declared: Vec<(String, &ServicePin)> = project
        .bougie
        .services
        .iter()
        .map(|(k, v)| (k.clone(), v))
        .collect();
    let selected: Vec<(String, &ServicePin)> = if names.is_empty() {
        declared
    } else {
        let mut out = Vec::new();
        for n in &names {
            if let Some((name, pin)) = declared.iter().find(|(k, _)| k == n) {
                out.push((name.clone(), *pin));
            } else {
                return Err(eyre!(
                    "service `{n}` isn't declared in this project (try `bougie services add {n}` first)"
                ));
            }
        }
        out
    };
    if selected.is_empty() {
        emit(format, &ServicesUpResult {
            schema_version: 2,
            started: vec![],
            tenants: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        })?;
        return Ok(ExitCode::SUCCESS);
    }

    // Default tenant: composer.json `name` field, slash → underscore.
    // Falls back to project dir basename when composer.json is absent
    // or carries no `name`.
    let default_tenant = derive_default_tenant(&project_root, project.composer.as_ref());

    let services_payload: Vec<Value> = selected
        .iter()
        .map(|(name, pin)| {
            let tenant = pin
                .tenant()
                .map(str::to_owned)
                .unwrap_or_else(|| default_tenant.clone());
            json!({"name": name, "tenant": tenant})
        })
        .collect();
    let args = json!({
        "project": project_root,
        "services": services_payload,
    });

    let paths = Paths::from_env()?;
    let reply: DaemonReply = client::call(&paths, "service.up", args)?;
    let result = ServicesUpResult {
        schema_version: 2,
        started: reply.started,
        tenants: reply.tenants,
        dependencies: reply.dependencies,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

fn derive_default_tenant(
    project_root: &std::path::Path,
    composer: Option<&crate::config::ComposerJson>,
) -> String {
    // composer.json's `name` was excluded from ComposerJson's struct,
    // so re-read it. Falls back to cwd basename on any parse error.
    if let Some(_c) = composer {
        let path = project_root.join("composer.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if let Some(name) = v.get("name").and_then(Value::as_str) {
                    return sanitize_tenant(name);
                }
            }
        }
    }
    let base = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    sanitize_tenant(base)
}

fn sanitize_tenant(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => out.push(c.to_ascii_lowercase()),
            _ => out.push('_'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_normalises_slash_and_dash() {
        assert_eq!(sanitize_tenant("acme/blog"), "acme_blog");
        assert_eq!(sanitize_tenant("My-Project"), "my_project");
        assert_eq!(sanitize_tenant("ACME"), "acme");
    }
}
