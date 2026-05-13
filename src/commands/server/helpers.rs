//! Implementations of `bougie server add/remove/list`. These shape
//! server.toml from the command line. Live pool status (the "is the
//! server running" half of `list`) lands in phase 6 with the control
//! socket; phase 0 prints config-only output.

use crate::cli::OutputFormat;
use crate::output::{emit, Render};
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::config;

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
    project: &Path,
    root: Option<&str>,
) -> Result<ExitCode> {
    let path = config::resolve_path(None)?;
    let added = config::add_host(&path, hostname, project, root)?;
    let result = AddResult {
        schema_version: 1,
        config: path,
        hostname: hostname.to_owned(),
        project: project.to_path_buf(),
        root: root.unwrap_or(".").to_owned(),
        added,
    };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
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
        config: path,
        hostname: hostname.to_owned(),
        removed,
    };
    emit(format, field, &result)?;
    Ok(if removed { ExitCode::SUCCESS } else { ExitCode::from(1) })
}

#[derive(Debug, Serialize)]
pub struct ListResult {
    pub schema_version: u32,
    pub config: PathBuf,
    pub hosts: Vec<ListedHost>,
}

#[derive(Debug, Serialize)]
pub struct ListedHost {
    pub hostname: String,
    pub project: PathBuf,
    pub root: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

impl Render for ListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.hosts.is_empty() {
            writeln!(w, "no hosts configured ({})", self.config.display())?;
            return Ok(());
        }
        for h in &self.hosts {
            writeln!(w, "{}  {}  root={}", h.hostname, h.project.display(), h.root)?;
            for alias in &h.aliases {
                writeln!(w, "  alias {alias}")?;
            }
        }
        Ok(())
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
    let result = ListResult { schema_version: 1, config: path, hosts };
    emit(format, field, &result)?;
    Ok(ExitCode::SUCCESS)
}
