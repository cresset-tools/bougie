//! `bougie service status [<name>]`. CLI.md §3.8.7.
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
    /// Human-readable binding for the text table: a resolved absolute
    /// socket path (`socket <path>`) or `tcp 127.0.0.1:<port>`. Derived
    /// CLI-side from `binding` + `Paths` because the daemon only knows
    /// the bare `sockname`, not the caller's `$BOUGIE_HOME`. The
    /// machine-readable form stays in `binding`, so this is skipped in
    /// JSON output.
    #[serde(skip)]
    pub binding_display: String,
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
    /// Consecutive failed continuous-health probes. `0` when healthy;
    /// non-zero means the service is failing its probe and counting down
    /// to a teardown-and-restart.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub health_misses: u64,
    /// Consecutive-miss threshold at which an unhealthy service is
    /// restarted (so the text view can render `2/3`).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub health_threshold: u64,
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
            // Binding (a resolved socket path / tcp target) goes after the
            // aligned columns; the crash/health note trails it so a
            // crash-looping, given-up, or wedged service is visible at a
            // glance instead of being buried in the state word.
            let note = status_note(row);
            writeln!(
                w,
                "{:14} {:15} pid={:>7} uptime={:>8}  {}{note}",
                row.name, row.state, pid, uptime, row.binding_display
            )?;
        }
        Ok(())
    }
}

/// A human-readable suffix describing crash/health state, or empty for a
/// plain healthy/stopped service. Drives the visibility for the two
/// failure modes `bougie up`/`start` otherwise hide: crash loops and
/// alive-but-wedged services.
fn status_note(row: &ServiceRow) -> String {
    match row.state.as_str() {
        // Failing its continuous health probe but still up.
        "unhealthy" if row.health_misses > 0 => {
            let threshold = if row.health_threshold > 0 {
                row.health_threshold
            } else {
                row.health_misses
            };
            format!("  ⚠ health check failing ({}/{threshold})", row.health_misses)
        }
        "failed" => {
            if let Some(ms) = row.next_restart_ms {
                #[allow(clippy::cast_precision_loss)]
                let secs = ms as f64 / 1000.0;
                format!(
                    "  ↻ crashed — restarting in {secs:.0}s (failure #{})",
                    row.failure_count
                )
            } else if row.failure_count > 0 {
                format!(
                    "  ✗ gave up after {} restart attempts — see `bougie service logs {}`",
                    row.failure_count, row.name
                )
            } else {
                format!("  ✗ failed to start — see `bougie service logs {}`", row.name)
            }
        }
        _ => String::new(),
    }
}

/// Render the supervisor's `binding` JSON (the serialized
/// [`Binding`](bougie_daemon::daemon::catalog::Binding) enum, internally
/// tagged with `kind`) as a copy-pasteable connection target for the
/// text table. TCP becomes `tcp 127.0.0.1:<port>`; a unix socket is
/// resolved to its absolute path (`socket <path>`) under
/// `$BOUGIE_HOME/state/services/<name>/run/<sockname>`, since the bare
/// `sockname` the daemon reports isn't enough to connect with. A `none`
/// binding, a `null`, or any shape we don't recognize renders as `-`.
fn format_binding(paths: &Paths, name: &str, binding: &Value) -> String {
    match binding.get("kind").and_then(Value::as_str) {
        Some("tcp") => binding
            .get("port")
            .and_then(Value::as_u64)
            .map_or_else(|| "-".into(), |p| format!("tcp 127.0.0.1:{p}")),
        Some("unix_socket") => binding
            .get("sockname")
            .and_then(Value::as_str)
            .map_or_else(
                || "-".into(),
                |s| format!("socket {}", paths.service_run(name, bougie_daemon::daemon::catalog::default_version(name)).join(s).display()),
            ),
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
            let binding = v.get("binding").cloned().unwrap_or(Value::Null);
            let binding_display = format_binding(&paths, &name, &binding);
            Some(ServiceRow {
                declared: declared.contains(name.as_str()),
                state: v
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                pid: v.get("pid").and_then(Value::as_u64),
                uptime_ms: v.get("uptime_ms").and_then(Value::as_u64),
                binding,
                binding_display,
                failure_count: v.get("failure_count").and_then(Value::as_u64).unwrap_or(0),
                next_restart_ms: v.get("next_restart_ms").and_then(Value::as_u64),
                health_misses: v.get("health_misses").and_then(Value::as_u64).unwrap_or(0),
                health_threshold: v.get("health_threshold").and_then(Value::as_u64).unwrap_or(0),
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
    use std::path::PathBuf;

    fn test_paths() -> Paths {
        Paths::new(PathBuf::from("/h"), PathBuf::from("/c"))
    }

    /// A minimal row for the crash/health-annotation tests: name + state,
    /// everything else defaulted.
    fn row(name: &str, state: &str) -> ServiceRow {
        ServiceRow {
            name: name.into(),
            state: state.into(),
            pid: None,
            uptime_ms: None,
            binding: Value::Null,
            binding_display: String::new(),
            declared: true,
            failure_count: 0,
            next_restart_ms: None,
            health_misses: 0,
            health_threshold: 0,
        }
    }

    /// A row wired with a resolved binding display, for the binding-column
    /// tests.
    fn binding_row(paths: &Paths, name: &str, binding: Value) -> ServiceRow {
        let binding_display = format_binding(paths, name, &binding);
        ServiceRow {
            name: name.into(),
            state: "running".into(),
            pid: Some(4242),
            uptime_ms: Some(12_500),
            binding,
            binding_display,
            declared: true,
            failure_count: 0,
            next_restart_ms: None,
            health_misses: 0,
            health_threshold: 0,
        }
    }

    #[test]
    fn healthy_running_service_has_no_note() {
        assert_eq!(status_note(&row("redis", "running")), "");
        assert_eq!(status_note(&row("redis", "stopped")), "");
    }

    #[test]
    fn unhealthy_service_shows_failing_probe_count() {
        let mut r = row("opensearch", "unhealthy");
        r.health_misses = 2;
        r.health_threshold = 3;
        let note = status_note(&r);
        assert!(note.contains("health check failing"), "{note}");
        assert!(note.contains("2/3"), "{note}");
    }

    #[test]
    fn crash_looping_service_shows_pending_restart() {
        let mut r = row("redis", "failed");
        r.failure_count = 3;
        r.next_restart_ms = Some(4000);
        let note = status_note(&r);
        assert!(note.contains("restarting in 4s"), "{note}");
        assert!(note.contains("failure #3"), "{note}");
    }

    #[test]
    fn given_up_service_points_at_logs() {
        let mut r = row("mariadb", "failed");
        r.failure_count = 10;
        r.next_restart_ms = None; // past the attempt cap → no respawn
        let note = status_note(&r);
        assert!(note.contains("gave up"), "{note}");
        assert!(note.contains("bougie service logs mariadb"), "{note}");
    }

    #[test]
    fn failed_to_start_service_points_at_logs_without_claiming_giveup() {
        // Initial-probe failure: Failed, but failure_count 0 and no
        // scheduled restart. Must not read "gave up after 0 attempts".
        let note = status_note(&row("opensearch", "failed"));
        assert!(note.contains("failed to start"), "{note}");
        assert!(!note.contains("gave up"), "{note}");
        assert!(note.contains("bougie service logs opensearch"), "{note}");
    }

    #[test]
    fn format_binding_covers_every_kind() {
        let paths = test_paths();
        assert_eq!(
            format_binding(&paths, "opensearch", &json!({"kind": "tcp", "port": 9200})),
            "tcp 127.0.0.1:9200"
        );
        // Unix sockets resolve to the absolute per-service run path.
        assert_eq!(
            format_binding(&paths, "redis", &json!({"kind": "unix_socket", "sockname": "redis.sock"})),
            "socket /h/state/services/redis/8.6.3/run/redis.sock"
        );
        // `none`, a bare null, and unknown shapes all degrade to `-`.
        assert_eq!(format_binding(&paths, "x", &json!({"kind": "none"})), "-");
        assert_eq!(format_binding(&paths, "x", &Value::Null), "-");
        assert_eq!(format_binding(&paths, "x", &json!({"kind": "tcp"})), "-");
    }

    #[test]
    fn render_text_includes_the_binding_column() {
        let paths = test_paths();
        let result = ServicesStatusResult {
            schema_version: 1,
            project: "/proj".into(),
            services: vec![
                binding_row(&paths, "mariadb", json!({"kind": "unix_socket", "sockname": "mariadb.sock"})),
                binding_row(&paths, "opensearch", json!({"kind": "tcp", "port": 9200})),
            ],
        };
        let mut buf = Vec::new();
        result.render_text(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("socket /h/state/services/mariadb/11.4.4/run/mariadb.sock"),
            "text: {text}"
        );
        assert!(text.contains("tcp 127.0.0.1:9200"), "text: {text}");
        // Existing columns survive alongside the new one.
        assert!(text.contains("pid=   4242"), "text: {text}");
    }

    #[test]
    fn binding_display_is_text_only_not_json() {
        let paths = test_paths();
        let result = ServicesStatusResult {
            schema_version: 1,
            project: "/proj".into(),
            services: vec![binding_row(
                &paths,
                "redis",
                json!({"kind": "unix_socket", "sockname": "redis.sock"}),
            )],
        };
        let json = serde_json::to_string(&result).unwrap();
        // The resolved display string is for humans only; JSON keeps the
        // machine-readable `binding` and never the host-specific path.
        assert!(!json.contains("binding_display"), "json: {json}");
        assert!(!json.contains("/h/state/services"), "json leaked path: {json}");
        assert!(json.contains("\"sockname\":\"redis.sock\""), "json: {json}");
    }
}
