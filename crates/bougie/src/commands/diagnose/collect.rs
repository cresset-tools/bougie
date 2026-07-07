//! Section collectors for the diagnose report. Offline-first: disk
//! state, logs, and config are read directly; the daemon is consulted
//! only when already running (never spawned). Every collector
//! degrades to an absent section rather than failing the report.

use super::scrub::Scrubber;
use crate::failure::LastFailure;
use bougie_paths::Paths;
use serde::Serialize;
use std::path::Path;

/// Per-log tail budget: at most this many lines …
pub const LOG_TAIL_LINES: usize = 200;
/// … and at most this many bytes (newest lines win). Only the Unix
/// collectors read logs, so the byte cap is gated with them (Windows
/// builds deny unused items).
#[cfg(unix)]
pub const LOG_TAIL_BYTES: usize = 16 * 1024;

#[derive(Debug, Serialize)]
pub struct DiagnoseReport {
    pub schema_version: u32,
    pub bougie_version: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_sha: Option<&'static str>,
    pub os: &'static str,
    pub arch: &'static str,
    pub libc: &'static str,
    pub telemetry_mode: &'static str,
    /// *Names* of the BOUGIE_* variables set — never their values.
    pub bougie_env_names: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<LastFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerun: Option<RerunCapture>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon: Option<DaemonInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ServiceInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortInfo>,
}

#[derive(Debug, Serialize)]
pub struct RerunCapture {
    pub argv: Vec<String>,
    pub exit_code: Option<i32>,
    pub stderr_tail: String,
}

#[derive(Debug, Serialize)]
pub struct ProjectInfo {
    pub root: String,
    pub declared_services: Vec<DeclaredService>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_free_home: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_free_cache: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DeclaredService {
    pub name: String,
    pub pin: String,
}

#[derive(Debug, Serialize)]
pub struct DaemonInfo {
    pub running: bool,
    pub log_tail: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ServiceInfo {
    pub name: String,
    pub pin: String,
    /// Live supervisor state (`running`, `failed`, …). Absent when
    /// the daemon isn't running — that state is memory-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Human crash/health context (restart counts, give-up notice).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_note: Option<String>,
    /// Rendered binding line, conflict verdict included.
    pub binding: String,
    pub log_tail: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub host: String,
    pub log_tail: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct PortInfo {
    pub port: u16,
    pub service: String,
    /// Extra-port qualifier (`epmd`, `web ui`); empty for the
    /// service's main binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<&'static str>,
    pub in_use: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    /// True when the port being in use is our own running service —
    /// i.e. not a conflict.
    pub expected: bool,
}

pub fn collect(
    paths: &Paths,
    failure: Option<LastFailure>,
    rerun: Option<RerunCapture>,
    project_root: Option<&Path>,
    scrub: &Scrubber,
) -> DiagnoseReport {
    let failure = failure.map(|mut f| {
        f.argv = f.argv.iter().map(|a| scrub.scrub(a)).collect();
        f.chain = f.chain.iter().map(|c| scrub.scrub(c)).collect();
        f.project_dir = None; // the project section already carries it, scrubbed
        f
    });
    let rerun = rerun.map(|mut r| {
        r.argv = r.argv.iter().map(|a| scrub.scrub(a)).collect();
        r.stderr_tail = scrub.scrub(&r.stderr_tail);
        r
    });
    let mode_file = bougie_paths::telemetry_mode_file().ok();
    let mut env_names: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| k.starts_with("BOUGIE_"))
        .collect();
    env_names.sort();

    let project = project_root.map(|root| project_info(paths, root, scrub));

    #[cfg(unix)]
    let (daemon, services, server, ports) = unix_sections(paths, project_root, scrub);
    #[cfg(not(unix))]
    let (daemon, services, server, ports) = (None, Vec::new(), None, Vec::new());

    DiagnoseReport {
        schema_version: 2,
        bougie_version: env!("CARGO_PKG_VERSION"),
        build_sha: bougie_cli::BUILD_SHA,
        os: bougie_telemetry::event::os(),
        arch: bougie_telemetry::event::arch(),
        libc: bougie_telemetry::event::libc(),
        telemetry_mode: bougie_telemetry::mode::resolve_from_env(mode_file.as_deref())
            .mode
            .as_str(),
        bougie_env_names: env_names,
        project,
        failure,
        rerun,
        daemon,
        services,
        server,
        ports,
    }
}

fn project_info(paths: &Paths, root: &Path, scrub: &Scrubber) -> ProjectInfo {
    let (declared, config_error) = match bougie_config::load_project(root) {
        Ok(project) => (
            project
                .bougie
                .services
                .iter()
                .map(|(name, pin)| DeclaredService {
                    name: name.clone(),
                    pin: pin.version().unwrap_or("*").to_owned(),
                })
                .collect(),
            None,
        ),
        Err(e) => (Vec::new(), Some(scrub.scrub(&format!("{e:#}")))),
    };
    ProjectInfo {
        root: scrub.scrub(&root.display().to_string()),
        declared_services: declared,
        config_error,
        disk_free_home: disk_free(paths.state().as_path()),
        disk_free_cache: disk_free(paths.cache()),
    }
}

/// Free space on the filesystem holding `path`, human-formatted.
/// Walks up to the nearest existing ancestor so a fresh install
/// (no state dir yet) still reports the disk.
#[cfg(unix)]
fn disk_free(path: &Path) -> Option<String> {
    let existing = path.ancestors().find(|p| p.exists())?;
    let vfs = rustix::fs::statvfs(existing).ok()?;
    #[allow(clippy::cast_precision_loss)]
    let gib = (vfs.f_bavail.saturating_mul(vfs.f_frsize)) as f64 / f64::from(1 << 30);
    Some(format!("{gib:.1} GiB"))
}

#[cfg(not(unix))]
fn disk_free(_path: &Path) -> Option<String> {
    None
}

// ---------- services / daemon / ports (Unix: the daemon stack) ----------

#[cfg(unix)]
fn unix_sections(
    paths: &Paths,
    project_root: Option<&Path>,
    scrub: &Scrubber,
) -> (
    Option<DaemonInfo>,
    Vec<ServiceInfo>,
    Option<ServerInfo>,
    Vec<PortInfo>,
) {
    use crate::commands::service::client;
    use bougie_daemon::daemon::catalog;
    use std::collections::HashMap;

    #[derive(Debug, serde::Deserialize)]
    struct StatusReply {
        services: Vec<serde_json::Value>,
    }

    // One read-only status round-trip, only if bougied is already up.
    let live: Option<HashMap<String, serde_json::Value>> =
        client::try_call::<StatusReply>(paths, "status", serde_json::Value::Null).map(|reply| {
            reply
                .services
                .into_iter()
                .filter_map(|v| {
                    let name = v
                        .get("name")
                        .and_then(serde_json::Value::as_str)?
                        .to_owned();
                    Some((name, v))
                })
                .collect()
        });

    let daemon = Some(DaemonInfo {
        running: live.is_some(),
        log_tail: tail_file(&paths.bougied_log(), scrub),
    });

    let declared: Vec<(String, String)> = project_root
        .and_then(|root| bougie_config::load_project(root).ok())
        .map(|project| {
            project
                .bougie
                .services
                .iter()
                .map(|(name, pin)| (name.clone(), pin.version().unwrap_or("*").to_owned()))
                .collect()
        })
        .unwrap_or_default();

    let mut services = Vec::new();
    let mut ports = Vec::new();
    for (name, pin) in &declared {
        let entry = catalog::find(name);
        let live_svc = live.as_ref().and_then(|m| m.get(name.as_str()));
        let state = live_svc
            .and_then(|v| v.get("state"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        // "The port is ours" — a live process in any serving/holding
        // state legitimately occupies the binding.
        let holds_binding = matches!(
            state.as_deref(),
            Some("running" | "unhealthy" | "health_checking" | "starting" | "stopping")
        );
        let binding = entry.map_or_else(
            || "(not in the service catalog)".to_owned(),
            |e| binding_line(paths, name, e.binding, holds_binding),
        );
        if let Some(e) = entry {
            if let catalog::Binding::Tcp { port } = e.binding {
                ports.push(probe_port(port, name, None, holds_binding));
            }
            for (port, purpose) in extra_ports(name) {
                ports.push(probe_port(*port, name, Some(purpose), holds_binding));
            }
        }
        services.push(ServiceInfo {
            name: name.clone(),
            pin: pin.clone(),
            status_note: live_svc.and_then(live_note),
            state,
            binding,
            log_tail: tail_file(&paths.service_log_file(name, bougie_daemon::daemon::catalog::default_version(name)), scrub),
        });
    }

    let server = project_root.and_then(|root| server_info(paths, root, scrub));

    (daemon, services, server, ports)
}

/// Sidecar / rides-along ports probed in addition to a service's
/// catalog binding.
#[cfg(unix)]
fn extra_ports(name: &str) -> &'static [(u16, &'static str)] {
    match name {
        // epmd is rabbitmq's in-group sidecar; a squatted 4369 breaks
        // node registration even when 5672 itself is free.
        "rabbitmq" => &[(4369, "epmd")],
        "mailpit" => &[(bougie_daemon::daemon::catalog::MAILPIT_HTTP_PORT, "web ui")],
        _ => &[],
    }
}

#[cfg(unix)]
fn probe_port(port: u16, service: &str, purpose: Option<&'static str>, expected: bool) -> PortInfo {
    use bougie_daemon::daemon::ports;
    let in_use = ports::port_in_use(port);
    PortInfo {
        port,
        service: service.to_owned(),
        purpose,
        in_use,
        holder: if in_use {
            ports::holder_of(port).map(|h| format!("{} (pid {})", h.comm, h.pid))
        } else {
            None
        },
        expected: expected && in_use,
    }
}

#[cfg(unix)]
fn binding_line(
    paths: &Paths,
    name: &str,
    binding: bougie_daemon::daemon::catalog::Binding,
    holds_binding: bool,
) -> String {
    use bougie_daemon::daemon::{catalog::Binding, ports};
    match binding {
        Binding::Tcp { port } => {
            if !ports::port_in_use(port) {
                return format!("tcp 127.0.0.1:{port} — not listening");
            }
            if holds_binding {
                return format!("tcp 127.0.0.1:{port} — listening (this service)");
            }
            let holder = ports::holder_of(port).map_or_else(
                || "another process".to_owned(),
                |h| format!("{} (pid {})", h.comm, h.pid),
            );
            format!("tcp 127.0.0.1:{port} — **in use by {holder}**, not this service")
        }
        Binding::UnixSocket { sockname } => {
            let path = paths.service_run(name, bougie_daemon::daemon::catalog::default_version(name)).join(sockname);
            let exists = if path.exists() { "present" } else { "absent" };
            format!("unix socket {} — {exists}", path.display())
        }
        Binding::None => "-".to_owned(),
    }
}

/// Crash/health context from a live status row, mirroring the notes
/// `bougie service status` prints.
#[cfg(unix)]
fn live_note(v: &serde_json::Value) -> Option<String> {
    let get = |k: &str| v.get(k).and_then(serde_json::Value::as_u64);
    let state = v.get("state").and_then(serde_json::Value::as_str)?;
    match state {
        "unhealthy" => {
            let misses = get("health_misses").unwrap_or(0);
            let threshold = get("health_threshold").filter(|t| *t > 0).unwrap_or(misses);
            Some(format!("health check failing ({misses}/{threshold})"))
        }
        "failed" => Some(
            match (get("next_restart_ms"), get("failure_count").unwrap_or(0)) {
                (Some(ms), n) => {
                    format!(
                        "crashed — restarting in {}s (failure #{n})",
                        ms.div_ceil(1000)
                    )
                }
                (None, n) if n > 0 => format!("gave up after {n} restart attempts"),
                (None, _) => "failed to start".to_owned(),
            },
        ),
        _ => None,
    }
}

/// The shared dev server's view of this project: its vhost plus a
/// host-filtered tail of the (shared, host-prefixed) server log.
#[cfg(unix)]
fn server_info(paths: &Paths, project_root: &Path, scrub: &Scrubber) -> Option<ServerInfo> {
    let conf = paths.service_conf("server", bougie_daemon::daemon::catalog::default_version("server")).join("server.toml");
    let config = bougie_server::server::config::load(&conf).ok()?;
    let canonical = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.into());
    let host = config
        .hosts
        .iter()
        .find(|h| h.project == project_root || h.project == canonical)?
        .hostname
        .clone();
    let lines = bougie_daemon::daemon::logs::tail_lines(&paths.service_log_file("server", bougie_daemon::daemon::catalog::default_version("server")), 1000)
        .unwrap_or_default()
        .into_iter()
        .filter(|l| l.contains(&host))
        .collect();
    Some(ServerInfo {
        host,
        log_tail: cap_lines(lines, scrub),
    })
}

#[cfg(unix)]
fn tail_file(path: &Path, scrub: &Scrubber) -> Vec<String> {
    let lines = bougie_daemon::daemon::logs::tail_lines(path, LOG_TAIL_LINES).unwrap_or_default();
    cap_lines(lines, scrub)
}

/// Newest-lines-win byte cap + scrub + newline trim.
#[cfg(unix)]
fn cap_lines(lines: Vec<String>, scrub: &Scrubber) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0usize;
    for line in lines.into_iter().rev() {
        let line = scrub.scrub(line.trim_end_matches(['\n', '\r']));
        total += line.len() + 1;
        if total > LOG_TAIL_BYTES {
            break;
        }
        kept.push(line);
    }
    kept.reverse();
    kept
}
