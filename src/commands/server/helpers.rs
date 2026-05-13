//! Implementations of `bougie server add/remove/list`. These shape
//! server.toml from the command line. Live pool status (the "is the
//! server running" half of `list`) lands in phase 6 with the control
//! socket; phase 0 prints config-only output.

use crate::cli::OutputFormat;
use crate::output::{emit, Render};
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::config;
use super::control::{self, LivePoolRow};
use super::hosts;
use super::paths::ServerPaths;

#[derive(Debug, Serialize)]
pub struct AddResult {
    pub schema_version: u32,
    pub config: PathBuf,
    pub hostname: String,
    pub project: PathBuf,
    pub root: String,
    /// `false` means "already present, no change".
    pub added: bool,
}

impl Render for AddResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.added {
            writeln!(
                w,
                "added {} -> {} (root={}) in {}",
                self.hostname,
                self.project.display(),
                self.root,
                self.config.display(),
            )
        } else {
            writeln!(w, "host {} already configured in {}", self.hostname, self.config.display())
        }
    }
}

pub fn add(
    format: OutputFormat,
    field: Option<&str>,
    hostname: &str,
    project: Option<&Path>,
    root: Option<&str>,
) -> Result<ExitCode> {
    // Auto-detect when the user didn't pass a project path. The detected
    // path goes through the same canonicalize-or-lexically-clean
    // pipeline as a literal argument, so the success message + stored
    // value still reflect the canonical form.
    let project_arg: PathBuf = match project {
        Some(p) => p.to_path_buf(),
        None => {
            let cwd = std::env::current_dir().wrap_err("getting cwd")?;
            let detected = auto_detect_project(&cwd).wrap_err(
                "no project path given and auto-detection from cwd failed",
            )?;
            eprintln!("bougie: auto-detected project {}", detected.display());
            detected
        }
    };
    let path = config::resolve_path(None)?;
    let canonical = config::add_host(&path, hostname, &project_arg, root)?;
    // Echo the canonical path back so users see what actually landed
    // in server.toml — `bougie server add myapp .` should not display
    // the literal `.`.
    let (stored, added) = match canonical.clone() {
        Some(c) => (c, true),
        None => (project_arg.clone(), false),
    };
    let result = AddResult {
        schema_version: 1,
        config: path.clone(),
        hostname: hostname.to_owned(),
        project: stored.clone(),
        root: root.unwrap_or(".").to_owned(),
        added,
    };
    emit(format, field, &result)?;
    if added {
        // Validate against the live config (which now reflects the new
        // entry) so the user gets a heads-up if pub/, public/, etc.
        // doesn't exist or doesn't contain an index file. Warning only;
        // the user might be wiring up a future project.
        if let Ok(cfg) = config::load(&path)
            && let Some(host_block) = cfg.hosts.iter().find(|h| h.hostname == hostname)
        {
            warn_host(host_block);
        }
        maybe_auto_apply_hosts(&path);
    }
    Ok(ExitCode::SUCCESS)
}

/// Print every validation warning for a `[[host]]` entry to stderr.
/// Shared between `bougie server add` and `bougie server run`.
pub fn warn_host(host: &config::HostBlock) {
    for w in config::validate_host(host) {
        eprintln!("warning: host {}: {}", host.hostname, w.render());
    }
}

/// Walk up from `cwd` looking for a project marker. Returns the
/// shallowest ancestor that contains `composer.json`, `bougie.toml`,
/// or a `.bougie/` directory. The intent is "you cd'd into your
/// project and ran `bougie server add` — figure out what you meant".
pub fn auto_detect_project(cwd: &Path) -> Result<PathBuf> {
    for ancestor in cwd.ancestors() {
        if ancestor.join("composer.json").is_file()
            || ancestor.join("bougie.toml").is_file()
            || ancestor.join(".bougie").is_dir()
        {
            return Ok(ancestor.to_path_buf());
        }
    }
    Err(eyre::eyre!(
        "no project marker (composer.json, bougie.toml, or .bougie/) found in {} or any parent",
        cwd.display()
    ))
}

#[derive(Debug, Serialize)]
pub struct RemoveResult {
    pub schema_version: u32,
    pub config: PathBuf,
    pub hostname: String,
    /// `false` means no matching entry was present.
    pub removed: bool,
}

impl Render for RemoveResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.removed {
            writeln!(w, "removed {} from {}", self.hostname, self.config.display())
        } else {
            writeln!(w, "no host {} in {}", self.hostname, self.config.display())
        }
    }
}

pub fn remove(format: OutputFormat, field: Option<&str>, hostname: &str) -> Result<ExitCode> {
    let path = config::resolve_path(None)?;
    let removed = config::remove_host(&path, hostname)?;
    let result = RemoveResult {
        schema_version: 1,
        config: path.clone(),
        hostname: hostname.to_owned(),
        removed,
    };
    emit(format, field, &result)?;
    if removed {
        maybe_auto_apply_hosts(&path);
    }
    Ok(if removed { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub schema_version: u32,
    pub config: PathBuf,
    pub hosts: Vec<ListedHost>,
    /// Live block populated when a server is running on this user's
    /// control socket. Absent when no server is running — keeps the
    /// json-v1 shape forward-compatible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveBlock>,
}

#[derive(Debug, Serialize)]
pub struct ListedHost {
    pub hostname: String,
    pub project: PathBuf,
    pub root: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct LiveBlock {
    pub listen_port: u16,
    pub pools: Vec<LivePoolRow>,
}

impl Render for ListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.hosts.is_empty() {
            writeln!(w, "no hosts configured ({})", self.config.display())?;
        } else {
            for h in &self.hosts {
                writeln!(w, "{}  {}  root={}", h.hostname, h.project.display(), h.root)?;
                for alias in &h.aliases {
                    writeln!(w, "  alias {alias}")?;
                }
            }
        }
        match &self.live {
            None => {
                writeln!(w, "(no server running on this user's control socket)")?;
            }
            Some(live) => {
                writeln!(w)?;
                writeln!(
                    w,
                    "running on :{} ({} pools)",
                    live.listen_port,
                    live.pools.len()
                )?;
                for p in &live.pools {
                    writeln!(
                        w,
                        "  {} [{}] pid={} php={} idle={}ms uptime={}ms",
                        p.project.display(),
                        p.variant,
                        p.pid,
                        p.php_version,
                        p.idle_ms,
                        p.started_ago_ms,
                    )?;
                }
            }
        }
        Ok(())
    }
}

/// If `[server].manage_etc_hosts` is on, spawn `sudo bougie server
/// hosts apply` to re-sync `/etc/hosts`. Errors are non-fatal: the
/// server.toml mutation is already committed, so we surface an
/// actionable hint and return.
///
/// Skipped entirely when bougie is already running as root — that
/// happens when the user runs `bougie server add` itself under sudo,
/// in which case spawning a nested sudo would prompt twice for no
/// reason. The root-flag check is also what makes the
/// `tests/server_helpers.rs` integration tests work: they run as the
/// user, so the flag-off path is exercised.
fn maybe_auto_apply_hosts(config_path: &Path) {
    let cfg = match config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "bougie: server.toml updated, but reloading it failed: {e:#}"
            );
            return;
        }
    };
    if !cfg.server.manage_etc_hosts {
        return;
    }
    match hosts::spawn_sudo_apply(config_path) {
        Ok(true) => {}
        Ok(false) => hosts::print_sudo_failure_hint(config_path),
        Err(e) => {
            eprintln!("bougie: failed to spawn sudo: {e:#}");
            hosts::print_sudo_failure_hint(config_path);
        }
    }
}

pub fn list(format: OutputFormat, field: Option<&str>) -> Result<ExitCode> {
    let path = config::resolve_path(None)?;
    let cfg = config::load(&path)?;
    let hosts = cfg
        .hosts
        .into_iter()
        .map(|h| ListedHost {
            hostname: h.hostname,
            project: h.project,
            root: h.root,
            aliases: h.aliases.into_iter().map(|a| a.hostname).collect(),
        })
        .collect();
    let live = query_live_status();
    let result = ListResult { schema_version: 1, config: path, hosts, live };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Try to query the running server's control socket. Returns `None`
/// silently when the socket is missing or the connect fails — the
/// `bougie server list` UX promises a graceful fallback to config-only
/// output when no server is running.
fn query_live_status() -> Option<LiveBlock> {
    let server_paths = ServerPaths::from_env().ok()?;
    let socket = server_paths.control_socket();
    let status = control::try_query_status(&socket)?;
    if !status.ok {
        return None;
    }
    Some(LiveBlock {
        listen_port: status.listen_port,
        pools: status.pools,
    })
}
