//! Implementations of the user-facing `bougie server` subcommands.
//!
//! Host registration (`add`/`remove`) was retired in favour of the
//! bougied-managed path: `bougie services up server` provisions a
//! `[[host]]` block per project tenant under
//! `$BOUGIE_HOME/state/services/server/conf/server.toml`. Users who
//! want to run the server by hand author their own `server.toml` and
//! invoke `bougie server run --config <path>` directly.
//!
//! This module now only ships `list` (the read-only inspector) and a
//! validation-warning helper shared with `server run`.

use bougie_cli::OutputFormat;
use bougie_output::output::{emit, Render};
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use super::config;
use super::control::{self, LivePoolRow};
use super::paths::ServerPaths;

/// Print every validation warning for a `[[host]]` entry to stderr.
/// Used by `bougie server run` at startup so misconfigured hosts
/// surface before requests start hitting them.
pub fn warn_host(host: &config::HostBlock) {
    for w in config::validate_host(host) {
        eprintln!("warning: host {}: {}", host.hostname, w.render());
    }
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

pub fn list(format: OutputFormat) -> Result<ExitCode> {
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
    emit(format, &result)?;
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
