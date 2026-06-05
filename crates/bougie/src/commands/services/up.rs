//! `bougie up [<name>…]` — promoted to a top-level verb from its
//! original home as `bougie services up`. The module path keeps the
//! `services::up` name because the handler still belongs to the
//! services subsystem semantically; only the user-facing CLI surface
//! moved. See CLI.md §3.8.4.

use super::client;
use super::config_mut::locate_project_root;
use bougie_cli::OutputFormat;
use bougie_config::{load_project, ServicePin};
use bougie_daemon::daemon::store_fetch::ResolvedTool;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
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

pub fn run(format: OutputFormat, names: Vec<String>, detach: bool) -> Result<ExitCode> {
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

    // Default tenant: sanitized project dir basename, made unique +
    // stable against the on-disk ledgers. See `commands::tenant`.
    let paths = Paths::from_env()?;
    let default_tenant = crate::commands::tenant::derive_default_tenant(&project_root, &paths);

    let services_payload: Vec<Value> = selected
        .iter()
        .map(|(name, pin)| {
            let tenant = pin
                .tenant().map_or_else(|| default_tenant.clone(), str::to_owned);
            json!({"name": name, "tenant": tenant})
        })
        .collect();
    let args = json!({
        "project": project_root,
        "services": services_payload,
    });

    let reply: DaemonReply = client::call(&paths, "service.up", args)?;
    let result = ServicesUpResult {
        schema_version: 2,
        started: reply.started,
        tenants: reply.tenants,
        dependencies: reply.dependencies,
    };
    emit(format, &result)?;

    // Attach to the combined ("multilog") stream of the services we
    // brought up, the way `docker compose up` follows its containers.
    // Gated to an interactive text-mode invocation: a non-TTY run (CI,
    // `bougie up | …`) or `--format json-v1` would never want a blocking
    // follow, so those implicitly detach — as does an explicit
    // `--detach`. The follow runs until Ctrl-C, which only detaches the
    // CLI; the daemon keeps the services running. Recipe steps that
    // shell out to `bougie up <svc>` pass `--detach` so the build never
    // blocks here (see recipes/{magento,laravel,generic}.toml).
    let attach = !detach
        && matches!(format, OutputFormat::Text)
        && std::io::stdout().is_terminal();
    if attach {
        let follow: Vec<String> = selected.iter().map(|(n, _)| n.clone()).collect();
        if !follow.is_empty() {
            eprintln!(
                "attached to logs for {} — Ctrl-C to detach (services keep running); `bougie up -d` to skip",
                follow.join(", ")
            );
            let log_args = json!({
                "services": follow,
                "lines": 10,
                "follow": true,
                // `attach` already required a TTY, so colorize the
                // per-service prefixes; the daemon writes the ANSI codes.
                "color": true,
            });
            client::call_streaming(&paths, "service.logs", log_args)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

