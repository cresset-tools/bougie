//! `bougie services status [<name>]`. CLI.md §3.8.7.
//!
//! Walks the supervisor's `status` IPC reply and renders the project's
//! services. Cross-project view (`--all`-equivalent) is reserved for
//! Phase 3+.

use super::client;
use super::config_mut::locate_project_root;
use bougie_cli::OutputFormat;
use bougie_config::load_project;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
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
    /// Consecutive failure count from the supervisor's backoff
    /// tracker. `0` for healthy services; surfaced so users can
    /// inspect crash-loop state.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub failure_count: u64,
    /// Milliseconds until the supervisor will auto-respawn after a
    /// crash. `None` for services that aren't pending a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_restart_ms: Option<u64>,
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
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
                |ms| {
                    // u64 ms → f64 loses precision past 2^52 ms (~143k
                    // years of uptime), which won't happen.
                    #[allow(clippy::cast_precision_loss)]
                    let secs = ms as f64 / 1000.0;
                    format!("{secs:.1}s")
                },
            );
            let binding = format_binding(&row.binding);
            writeln!(
                w,
                "{:14} {:15} {:22} pid={:>7} uptime={:>8}",
                row.name, row.state, binding, pid, uptime
            )?;
        }
        Ok(())
    }
}

/// Render the supervisor's `binding` JSON (the serialized
/// [`Binding`](bougie_daemon::daemon::catalog::Binding) enum, internally
/// tagged with `kind`) as a compact address for the text table. Mirrors
/// the wording of `bougie services catalog`. A `none` binding, a `null`,
/// or any shape we don't recognize renders as `-`.
fn format_binding(binding: &Value) -> String {
    match binding.get("kind").and_then(Value::as_str) {
        Some("tcp") => binding
            .get("port")
            .and_then(Value::as_u64)
            .map_or_else(|| "-".into(), |p| format!("tcp 127.0.0.1:{p}")),
        Some("unix_socket") => binding
            .get("sockname")
            .and_then(Value::as_str)
            .map_or_else(|| "-".into(), |s| format!("socket ({s})")),
        _ => "-".into(),
    }
}

pub fn run(format: OutputFormat, name: Option<String>) -> Result<ExitCode> {
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
                failure_count: v.get("failure_count").and_then(Value::as_u64).unwrap_or(0),
                next_restart_ms: v.get("next_restart_ms").and_then(Value::as_u64),
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
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

// no-op helper kept inline to silence clippy's "needless borrow" on
// the filter chain above; future Phase will use this for `--all`.
fn name_filter<'a>(_a: &'a str, _b: &'a str, _c: &'a str) -> Option<&'a str> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(name: &str, binding: Value) -> ServiceRow {
        ServiceRow {
            name: name.into(),
            state: "running".into(),
            pid: Some(4242),
            uptime_ms: Some(12_500),
            binding,
            declared: true,
            failure_count: 0,
            next_restart_ms: None,
        }
    }

    #[test]
    fn format_binding_covers_every_kind() {
        assert_eq!(
            format_binding(&json!({"kind": "tcp", "port": 9200})),
            "tcp 127.0.0.1:9200"
        );
        assert_eq!(
            format_binding(&json!({"kind": "unix_socket", "sockname": "redis.sock"})),
            "socket (redis.sock)"
        );
        // `none`, a bare null, and unknown shapes all degrade to `-`.
        assert_eq!(format_binding(&json!({"kind": "none"})), "-");
        assert_eq!(format_binding(&Value::Null), "-");
        assert_eq!(format_binding(&json!({"kind": "tcp"})), "-");
    }

    #[test]
    fn render_text_includes_the_binding_column() {
        let result = ServicesStatusResult {
            schema_version: 1,
            project: "/proj".into(),
            services: vec![
                row("mariadb", json!({"kind": "unix_socket", "sockname": "mariadb.sock"})),
                row("opensearch", json!({"kind": "tcp", "port": 9200})),
            ],
        };
        let mut buf = Vec::new();
        result.render_text(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("socket (mariadb.sock)"), "text: {text}");
        assert!(text.contains("tcp 127.0.0.1:9200"), "text: {text}");
        // Existing columns survive alongside the new one.
        assert!(text.contains("pid=   4242"), "text: {text}");
    }
}
