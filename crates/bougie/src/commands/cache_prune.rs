//! `bougie cache prune` — drops stale cached artifacts.
//!
//! Three walks (only the ephemeral-env ones are wired today):
//!
//! 1. **Store reachability walk** (Phase 8 work) — would remove
//!    extension `.so`s no live project + tool references. Deferred
//!    until `bougie ext` tracks per-project enabled extensions.
//! 2. **Tool-run cache walk** — drops `paths.cache_tool_run()/<key>/`
//!    entries whose `receipt.toml` mtime is older than the TTL.
//!    A `bougie tool run` cache hit refreshes the mtime so active
//!    tools aren't GC'd.
//! 3. **Script-run cache walk** — same shape for
//!    `paths.cache_script_run()/<key>/`, keyed on the
//!    `.bougie-script-ready` marker a `bougie run --script` hit
//!    refreshes.

use bougie_cli::OutputFormat;
use bougie_output::output::{Render, emit};
use bougie_paths::Paths;
use eyre::{Result, WrapErr};
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

/// Ephemeral-env cache TTL: 14 days. Slots not exec'd in this window get
/// pruned. Shared by the tool-run and script-run walks. CLI flag for
/// tuning lands in a follow-up.
// 14 days. `from_days` isn't const-stable yet so we compose via
// `from_hours` (which is).
const EPHEMERAL_TTL: Duration = Duration::from_hours(14 * 24);

/// Marker file whose mtime gates staleness for a tool-run slot.
const TOOL_RUN_MARKER: &str = "receipt.toml";
/// Marker file whose mtime gates staleness for a script-run slot.
const SCRIPT_RUN_MARKER: &str = ".bougie-script-ready";

#[derive(Debug, Serialize)]
pub struct PruneResult {
    pub schema_version: u32,
    pub dry_run: bool,
    /// Cache slots removed (or that would be removed under
    /// `--dry-run`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_run_pruned: Vec<PathBuf>,
    /// Script-run (`bougie run --script`) cache slots removed (or that
    /// would be removed under `--dry-run`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub script_run_pruned: Vec<PathBuf>,
    /// Reachability-walk status. Stays a string until the Phase 8
    /// store walk lands.
    pub store_walk: &'static str,
}

impl Render for PruneResult {
    fn render_text(&self, w: &mut dyn Write) -> io::Result<()> {
        self.render_cache(w, "tool-run cache", &self.tool_run_pruned)?;
        self.render_cache(w, "script-run cache", &self.script_run_pruned)?;
        writeln!(w, "store: {}", self.store_walk)?;
        Ok(())
    }
}

impl PruneResult {
    fn render_cache(&self, w: &mut dyn Write, label: &str, pruned: &[PathBuf]) -> io::Result<()> {
        if pruned.is_empty() {
            writeln!(w, "{label}: nothing to prune")?;
        } else {
            let verb = if self.dry_run { "would remove" } else { "removed" };
            writeln!(w, "{label}: {verb} {} slot(s)", pruned.len())?;
            for slot in pruned {
                writeln!(w, "  {}", slot.display())?;
            }
        }
        Ok(())
    }
}

pub fn run(format: OutputFormat, dry_run: bool) -> Result<ExitCode> {
    let paths = Paths::from_env()?;
    let tool_run_pruned = prune_cache(&paths.cache_tool_run(), TOOL_RUN_MARKER, dry_run)?;
    let script_run_pruned = prune_cache(&paths.cache_script_run(), SCRIPT_RUN_MARKER, dry_run)?;
    let result = PruneResult {
        schema_version: 3,
        dry_run,
        tool_run_pruned,
        script_run_pruned,
        store_walk: "skipped (reachability walk arrives with `bougie ext` tracking)",
    };
    emit(format, &result)?;
    Ok(ExitCode::SUCCESS)
}

/// Walk an ephemeral-env cache `root` and remove (or list, under
/// `--dry-run`) every slot whose `marker` mtime is older than
/// `EPHEMERAL_TTL`. Slots without the marker are treated as broken
/// scaffolding from an aborted materialisation and pruned alongside.
fn prune_cache(root: &Path, marker: &str, dry_run: bool) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(root) {
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
        if is_stale(&slot, marker, now)? {
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

fn is_stale(slot: &Path, marker: &str, now: SystemTime) -> Result<bool> {
    let marker_path = slot.join(marker);
    // No marker → leftover from a failed materialisation; prune.
    let Ok(meta) = std::fs::metadata(&marker_path) else {
        return Ok(true);
    };
    let modified = meta
        .modified()
        .wrap_err_with(|| format!("reading mtime of {}", marker_path.display()))?;
    let age = now
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    Ok(age > EPHEMERAL_TTL)
}
