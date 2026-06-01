//! `bougie tool list` — print installed tools, marking broken/stale
//! entries.

use bougie_cli::OutputFormat;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use bougie_tool::list::{ListedTool, ToolStatus, list};
use eyre::Result;
use serde::Serialize;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Serialize)]
pub struct ToolListResult {
    pub schema_version: u32,
    pub tools: Vec<ToolEntry>,
}

#[derive(Debug, Serialize)]
pub struct ToolEntry {
    pub dir_name: String,
    pub tool_dir: PathBuf,
    pub package: Option<String>,
    pub constraint: Option<String>,
    pub php_version: Option<String>,
    pub php_flavor: Option<String>,
    pub bins: Vec<String>,
    pub status: &'static str,
    pub reason: Option<String>,
}

impl From<&ListedTool> for ToolEntry {
    fn from(t: &ListedTool) -> Self {
        match &t.status {
            ToolStatus::Healthy(r) => Self {
                dir_name: t.dir_name.clone(),
                tool_dir: t.tool_dir.clone(),
                package: Some(r.package.clone()),
                constraint: Some(r.constraint.clone()),
                php_version: Some(r.php_version.clone()),
                php_flavor: Some(r.php_flavor.clone()),
                bins: r.entrypoints.iter().map(|e| e.name.clone()).collect(),
                status: "healthy",
                reason: None,
            },
            ToolStatus::Stale { receipt, reason } => Self {
                dir_name: t.dir_name.clone(),
                tool_dir: t.tool_dir.clone(),
                package: Some(receipt.package.clone()),
                constraint: Some(receipt.constraint.clone()),
                php_version: Some(receipt.php_version.clone()),
                php_flavor: Some(receipt.php_flavor.clone()),
                bins: receipt.entrypoints.iter().map(|e| e.name.clone()).collect(),
                status: "stale",
                reason: Some(reason.clone()),
            },
            ToolStatus::Broken { reason } => Self {
                dir_name: t.dir_name.clone(),
                tool_dir: t.tool_dir.clone(),
                package: None,
                constraint: None,
                php_version: None,
                php_flavor: None,
                bins: Vec::new(),
                status: "broken",
                reason: Some(reason.clone()),
            },
        }
    }
}

/// Synthetic `bougie tool list` entry for the built-in, project-aware
/// Composer. Composer isn't a normal tool (no receipt, no isolated
/// vendor tree) — it's reimplemented natively for `install`/`update`/
/// `dump-autoload`/`validate` and forwarded to the real phar for
/// everything else, always with the project's PHP. Surfacing it here
/// makes "composer is installed by default" visible.
fn composer_builtin_entry(paths: &Paths) -> ToolEntry {
    ToolEntry {
        dir_name: "composer-composer".to_string(),
        tool_dir: paths.tool_bin_dir().join("composer"),
        package: Some("composer/composer".to_string()),
        constraint: Some("stable".to_string()),
        php_version: None,
        php_flavor: None,
        bins: vec!["composer".to_string()],
        status: "built-in",
        reason: None,
    }
}

impl Render for ToolListResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.tools.is_empty() {
            return writeln!(w, "no tools installed");
        }
        for t in &self.tools {
            match t.status {
                "built-in" => writeln!(
                    w,
                    "{pkg} (built-in, project-aware; bins: {bins})",
                    pkg = t.package.as_deref().unwrap_or("?"),
                    bins = t.bins.join(", "),
                )?,
                "healthy" => writeln!(
                    w,
                    "{pkg} {ver} (php {php}, bins: {bins})",
                    pkg = t.package.as_deref().unwrap_or("?"),
                    ver = t.constraint.as_deref().unwrap_or("?"),
                    php = t.php_version.as_deref().unwrap_or("?"),
                    bins = if t.bins.is_empty() {
                        "(none)".into()
                    } else {
                        t.bins.join(", ")
                    },
                )?,
                "stale" => writeln!(
                    w,
                    "{pkg} {ver} — STALE: {reason}",
                    pkg = t.package.as_deref().unwrap_or("?"),
                    ver = t.constraint.as_deref().unwrap_or("?"),
                    reason = t.reason.as_deref().unwrap_or(""),
                )?,
                _ => writeln!(
                    w,
                    "{dir} — BROKEN: {reason}",
                    dir = t.dir_name,
                    reason = t.reason.as_deref().unwrap_or(""),
                )?,
            }
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let listed = list(&paths)?;
    // Composer leads the list as the always-present built-in tool.
    let mut tools = vec![composer_builtin_entry(&paths)];
    tools.extend(listed.iter().map(ToolEntry::from));
    let result = ToolListResult {
        schema_version: 1,
        tools,
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}
