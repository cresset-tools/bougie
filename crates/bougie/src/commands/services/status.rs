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
            // Trailing annotation so a crash-looping, given-up, or
            // wedged (failing-health-check) service is visible at a
            // glance instead of being silently buried in the state word.
            let note = status_note(row);
            writeln!(
                w,
                "{:14} {:15} pid={:>7} uptime={:>8}{note}",
                row.name, row.state, pid, uptime
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
                    "  ✗ gave up after {} restart attempts — see `bougie services logs {}`",
                    row.failure_count, row.name
                )
            } else {
                format!("  ✗ failed to start — see `bougie services logs {}`", row.name)
            }
        }
        _ => String::new(),
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

    fn row(name: &str, state: &str) -> ServiceRow {
        ServiceRow {
            name: name.into(),
            state: state.into(),
            pid: None,
            uptime_ms: None,
            binding: Value::Null,
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
        assert!(note.contains("bougie services logs mariadb"), "{note}");
    }

    #[test]
    fn failed_to_start_service_points_at_logs_without_claiming_giveup() {
        // Initial-probe failure: Failed, but failure_count 0 and no
        // scheduled restart. Must not read "gave up after 0 attempts".
        let note = status_note(&row("opensearch", "failed"));
        assert!(note.contains("failed to start"), "{note}");
        assert!(!note.contains("gave up"), "{note}");
        assert!(note.contains("bougie services logs opensearch"), "{note}");
    }
}
