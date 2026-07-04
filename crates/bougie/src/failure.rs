//! Last-failure capture for `bougie diagnose`.
//!
//! On any command error, `report_error` (main.rs) records the full,
//! *unscrubbed* error context into a single-slot local file —
//! `<cache>/telemetry/last-failure.json` — so `bougie diagnose` can
//! assemble a rich report after the fact with zero re-work. This is a
//! local artifact of the same class as a log file: it is written
//! regardless of the telemetry mode and nothing ever reads it off the
//! machine without the user reviewing and confirming a send.

use bougie_errors::exit_code_for;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastFailure {
    pub schema: u32,
    /// Unix seconds; full precision is fine for a local file.
    pub ts_epoch: u64,
    pub argv: Vec<String>,
    pub bougie_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_sha: Option<String>,
    /// Telemetry outcome category (closed set) for orientation.
    pub category: String,
    pub exit_code: u8,
    /// The eyre chain, outermost first, messages verbatim.
    pub chain: Vec<String>,
}

pub fn path(cache_root: &Path) -> PathBuf {
    cache_root.join("telemetry").join("last-failure.json")
}

/// Record the failure. Best-effort by contract — diagnostics capture
/// must never compound the original error.
pub fn record(err: &eyre::Report) {
    let Ok(paths) = bougie_paths::Paths::from_env() else { return };
    let failure = LastFailure {
        schema: 1,
        ts_epoch: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        argv: std::env::args().collect(),
        bougie_version: env!("CARGO_PKG_VERSION").to_owned(),
        build_sha: bougie_cli::BUILD_SHA.map(str::to_owned),
        category: bougie_telemetry::outcome_for_error(err).to_owned(),
        exit_code: exit_code_for(err),
        chain: err.chain().map(ToString::to_string).collect(),
    };
    let target = path(paths.cache());
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&failure) {
        let _ = std::fs::write(target, json);
    }
}

/// Load the recorded failure, if any.
pub fn load(cache_root: &Path) -> Option<LastFailure> {
    let raw = std::fs::read_to_string(path(cache_root)).ok()?;
    serde_json::from_str(&raw).ok()
}
