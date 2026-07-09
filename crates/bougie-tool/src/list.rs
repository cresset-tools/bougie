//! `bougie tool list` — enumerate installed tools by walking
//! `$BOUGIE_LOCAL/tools/*/receipt.toml`.
//!
//! A tool directory without a parseable receipt is surfaced as
//! `Broken`: the user picks recovery (`tool upgrade --reinstall` once
//! Phase 2 ships it, or a manual `tool uninstall` + reinstall). A tool
//! whose pinned PHP no longer exists on disk is `Stale` — same fix.

use crate::receipt::{self, ToolReceipt};
use bougie_paths::Paths;
use eyre::Result;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ListedTool {
    /// On-disk dir name (slash-replaced package, e.g. `phpstan-phpstan`).
    pub dir_name: String,
    pub tool_dir: PathBuf,
    pub status: ToolStatus,
}

#[derive(Debug, Clone)]
pub enum ToolStatus {
    Healthy(ToolReceipt),
    Stale {
        receipt: ToolReceipt,
        reason: String,
    },
    Broken {
        reason: String,
    },
}

pub fn list(paths: &Paths) -> Result<Vec<ListedTool>> {
    let root = paths.tools();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(eyre::Report::new(e).wrap_err(format!("reading {}", root.display()))),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let tool_dir = entry.path();
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let receipt_path = tool_dir.join("receipt.toml");
        let status = if receipt_path.exists() {
            match receipt::read(&receipt_path) {
                Ok(r) => check_health(&r),
                Err(e) => ToolStatus::Broken {
                    reason: format!("receipt corrupt: {e}"),
                },
            }
        } else {
            ToolStatus::Broken {
                reason: "receipt.toml missing".into(),
            }
        };
        out.push(ListedTool {
            dir_name,
            tool_dir,
            status,
        });
    }
    out.sort_by(|a, b| a.dir_name.cmp(&b.dir_name));
    Ok(out)
}

fn check_health(receipt: &ToolReceipt) -> ToolStatus {
    if !receipt.php_resolved_path.exists() {
        return ToolStatus::Stale {
            receipt: receipt.clone(),
            reason: format!(
                "pinned PHP {} no longer installed at {}",
                receipt.php_version,
                receipt.php_resolved_path.display()
            ),
        };
    }
    ToolStatus::Healthy(receipt.clone())
}
