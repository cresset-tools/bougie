//! `bougie cache prune` — drops stale cached artifacts.
//!
//! Two walks (only the second is wired today):
//!
//! 1. **Store reachability walk** (Phase 8 work) — would remove
//!    extension `.so`s no live project + tool references. Deferred
//!    until `bougie ext` tracks per-project enabled extensions.
//! 2. **Tool-run cache walk** — drops `paths.cache_tool_run()/<key>/`
//!    entries whose `receipt.toml` mtime is older than `TOOL_RUN_TTL`.
//!    A `bougie tool run` cache hit refreshes the mtime so active
//!    tools aren't GC'd.

use bougie_cli::OutputFormat;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

/// Tool-run cache TTL: 14 days. Slots not exec'd in this window get
/// pruned. CLI flag for tuning lands in a follow-up.
// 14 days. `from_days` isn't const-stable yet so we compose via
// `from_hours` (which is).
const TOOL_RUN_TTL: Duration = Duration::from_hours(14 * 24);

#[derive(Debug, Serialize)]
pub struct PruneResult {
    pub schema_version: u32,
    pub dry_run: bool,
    /// Cache slots removed (or that would be removed under
    /// `--dry-run`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_run_pruned: Vec<PathBuf>,
    /// Reachability-walk status. Stays a string until the Phase 8
    /// store walk lands.
    pub store_walk: &'static str,
}

impl Render for PruneResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.tool_run_pruned.is_empty() {
            writeln!(w, "tool-run cache: nothing to prune")?;
        } else {
            let verb = if self.dry_run { "would remove" } else { "removed" };
            writeln!(
                w,
                "tool-run cache: {verb} {} slot(s)",
                self.tool_run_pruned.len()
            )?;
            for slot in &self.tool_run_pruned {
                writeln!(w, "  {}", slot.display())?;
            }
        }
        writeln!(w, "store: {}", self.store_walk)?;
        Ok(())
    }
}

pub fn run(format: OutputFormat, dry_run: bool) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let pruned = prune_tool_run(&paths, dry_run)?;
    let result = PruneResult {
        schema_version: 2,
        dry_run,
        tool_run_pruned: pruned,
        store_walk: "skipped (reachability walk arrives with `bougie ext` tracking)",
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Walk `paths.cache_tool_run()` and remove (or list, under
/// `--dry-run`) every slot whose `receipt.toml` mtime is older than
/// `TOOL_RUN_TTL`. Slots without a receipt are treated as broken
/// scaffolding from an aborted materialisation and pruned alongside.
fn prune_tool_run(paths: &Paths, dry_run: bool) -> Result<Vec<PathBuf>> {
    let root = paths.cache_tool_run();
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(eyre::eyre!("reading {}: {e}", root.display())),
    };
    let now = SystemTime::now();
    let mut pruned = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let slot = entry.path();
        if is_stale(&slot, now)? {
            if !dry_run {
                std::fs::remove_dir_all(&slot)
                    .wrap_err_with(|| format!("removing {}", slot.display()))?;
            }
            pruned.push(slot);
        }
    }
    pruned.sort();
    Ok(pruned)
}

fn is_stale(slot: &Path, now: SystemTime) -> Result<bool> {
    let receipt = slot.join("receipt.toml");
    // No receipt → leftover from a failed materialisation; prune.
    let Ok(meta) = std::fs::metadata(&receipt) else {
        return Ok(true);
    };
    let modified = meta
        .modified()
        .wrap_err_with(|| format!("reading mtime of {}", receipt.display()))?;
    let age = now
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    Ok(age > TOOL_RUN_TTL)
}
