//! Failure-ring capture for `bougie diagnose`.
//!
//! On any command error, `report_error` (main.rs) records the full,
//! *unscrubbed* error context into a small local ring —
//! `<cache>/telemetry/failures/<epoch>-<pid>.json` — so `bougie
//! diagnose` can assemble a rich report after the fact with zero
//! re-work. A consecutive repeat of the *same* failure (a `watch`
//! loop, a retrying supervisor) collapses into the newest entry's
//! `repeats` counter instead of churning the ring, so a runaway burst
//! keeps its onset, its scale, and the nine failures before it on
//! disk. These are local artifacts of the same class as a log file:
//! written regardless of the telemetry mode, and nothing ever reads
//! them off the machine without the user reviewing and confirming a
//! send.
//!
//! Pre-ring versions wrote a single `telemetry/last-failure.json`
//! slot; reads fall back to it so the failure that motivated an
//! upgrade is still reportable.

use bougie_errors::exit_code_for;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Ring capacity. Small on purpose: with repeat-collapsing an entry
/// is a distinct failure, and ten distinct recent failures is plenty
/// of context for a report.
pub const RING_CAP: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastFailure {
    pub schema: u32,
    /// Unix seconds of the first occurrence; full precision is fine
    /// for a local file.
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
    /// Root of the project the failing command ran in, when one was
    /// found around the cwd (schema 2). Lets `bougie diagnose` report
    /// on the right project's services even when invoked from another
    /// directory. Schema-1 files simply lack the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<PathBuf>,
    /// How many consecutive occurrences this entry stands for
    /// (schema 3): an identical failure updates the newest ring entry
    /// in place instead of appending.
    #[serde(default = "default_repeats")]
    pub repeats: u64,
    /// Unix seconds of the latest repeat; absent while `repeats == 1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ts_epoch: Option<u64>,
}

fn default_repeats() -> u64 {
    1
}

impl LastFailure {
    /// The collapse test: same command failing the same way. The full
    /// chain participates so failures that differ only in a buried
    /// cause stay distinct entries.
    fn same_failure_as(&self, other: &Self) -> bool {
        self.argv == other.argv && self.category == other.category && self.chain == other.chain
    }
}

/// The ring directory.
pub fn ring_dir(cache_root: &Path) -> PathBuf {
    cache_root.join("telemetry").join("failures")
}

/// The pre-ring single-slot file, still read as a fallback.
pub fn legacy_path(cache_root: &Path) -> PathBuf {
    cache_root.join("telemetry").join("last-failure.json")
}

/// Record the failure. Best-effort by contract — diagnostics capture
/// must never compound the original error.
pub fn record(err: &eyre::Report) {
    let Ok(paths) = bougie_paths::Paths::from_env() else {
        return;
    };
    let failure = LastFailure {
        schema: 3,
        ts_epoch: now_epoch(),
        argv: std::env::args().collect(),
        bougie_version: env!("CARGO_PKG_VERSION").to_owned(),
        build_sha: bougie_cli::BUILD_SHA.map(str::to_owned),
        category: bougie_telemetry::outcome_for_error(err).to_owned(),
        exit_code: exit_code_for(err),
        chain: err.chain().map(ToString::to_string).collect(),
        project_dir: std::env::current_dir()
            .ok()
            .and_then(|cwd| project_root_near(&cwd)),
        repeats: 1,
        last_ts_epoch: None,
    };
    record_in(paths.cache(), &failure);
}

/// Testable core of [`record`]: collapse into the newest entry when
/// it's the same failure, else append and prune to [`RING_CAP`].
pub fn record_in(cache_root: &Path, failure: &LastFailure) {
    let dir = ring_dir(cache_root);
    let _ = std::fs::create_dir_all(&dir);

    if let Some(newest) = ring_files(&dir).into_iter().next_back()
        && let Some(mut prev) = read_entry(&newest)
        && prev.same_failure_as(failure)
    {
        prev.repeats = prev.repeats.saturating_add(1);
        prev.last_ts_epoch = Some(failure.ts_epoch);
        write_entry(&newest, &prev);
        return;
    }

    let name = format!("{:020}-{}.json", failure.ts_epoch, std::process::id());
    write_entry(&dir.join(name), failure);

    let files = ring_files(&dir);
    for stale in files.iter().take(files.len().saturating_sub(RING_CAP)) {
        let _ = std::fs::remove_file(stale);
    }
}

/// The newest recorded failure, if any — ring first, then the
/// pre-ring single slot.
pub fn load(cache_root: &Path) -> Option<LastFailure> {
    load_recent(cache_root).into_iter().next()
}

/// Recorded failures, newest first. Falls back to the pre-ring slot
/// when the ring is empty. Schema 1–3 files all parse (missing
/// fields default).
pub fn load_recent(cache_root: &Path) -> Vec<LastFailure> {
    let entries: Vec<LastFailure> = ring_files(&ring_dir(cache_root))
        .into_iter()
        .rev()
        .filter_map(|p| read_entry(&p))
        .collect();
    if !entries.is_empty() {
        return entries;
    }
    read_entry(&legacy_path(cache_root)).into_iter().collect()
}

/// Ring files sorted oldest → newest. The zero-padded epoch prefix
/// makes lexical order chronological.
fn ring_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
}

fn read_entry(path: &Path) -> Option<LastFailure> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_entry(path: &Path, failure: &LastFailure) {
    if let Ok(json) = serde_json::to_string_pretty(failure) {
        let _ = std::fs::write(path, json);
    }
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `2026-07-09 06:12:34 UTC` from unix seconds, for human-facing
/// listings of ring entries.
pub fn format_epoch(secs: u64) -> String {
    bougie_telemetry::clock::format_epoch_utc(secs)
}

/// Walk from `dir` upward for a project marker. Mirrors
/// `commands::service::config_mut::locate_project_root`, but
/// infallible and cross-platform (that helper lives behind
/// `cfg(unix)` with the services stack).
pub fn project_root_near(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|anc| {
            anc.join("bougie.toml").is_file()
                || anc.join("composer.json").is_file()
                || bougie_paths::project::is_root(anc)
        })
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts: u64, argv: &[&str], chain: &[&str]) -> LastFailure {
        LastFailure {
            schema: 3,
            ts_epoch: ts,
            argv: argv.iter().map(ToString::to_string).collect(),
            bougie_version: "0.0.0".into(),
            build_sha: None,
            category: "other".into(),
            exit_code: 1,
            chain: chain.iter().map(ToString::to_string).collect(),
            project_dir: None,
            repeats: 1,
            last_ts_epoch: None,
        }
    }

    #[test]
    fn identical_failures_collapse_into_repeats() {
        let dir = tempfile::tempdir().unwrap();
        for ts in 100..1655 {
            record_in(dir.path(), &entry(ts, &["bougie", "composer"], &["boom"]));
        }
        let recent = load_recent(dir.path());
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].repeats, 1555);
        assert_eq!(recent[0].ts_epoch, 100);
        assert_eq!(recent[0].last_ts_epoch, Some(1654));
    }

    #[test]
    fn distinct_failures_append_and_prune_to_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(RING_CAP as u64 + 5) {
            let arg = format!("cmd{i}");
            record_in(dir.path(), &entry(1000 + i, &["bougie", &arg], &["boom"]));
        }
        let recent = load_recent(dir.path());
        assert_eq!(recent.len(), RING_CAP);
        // Newest first; the oldest five were pruned.
        assert_eq!(recent[0].argv[1], format!("cmd{}", RING_CAP + 4));
        assert_eq!(recent.last().unwrap().argv[1], "cmd5");
    }

    #[test]
    fn burst_then_new_failure_keeps_both() {
        let dir = tempfile::tempdir().unwrap();
        for ts in 0..50 {
            record_in(dir.path(), &entry(ts, &["bougie", "composer"], &["boom"]));
        }
        record_in(dir.path(), &entry(60, &["bougie", "sync"], &["different"]));
        let recent = load_recent(dir.path());
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].argv[1], "sync");
        assert_eq!(recent[1].repeats, 50);
    }

    #[test]
    fn legacy_single_slot_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = legacy_path(dir.path());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        // A schema-2 file predating `repeats`/`last_ts_epoch`.
        std::fs::write(
            &legacy,
            r#"{"schema":2,"ts_epoch":5,"argv":["bougie","run"],"bougie_version":"0.45.0",
               "category":"other","exit_code":1,"chain":["old failure"]}"#,
        )
        .unwrap();
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.repeats, 1);
        assert_eq!(loaded.chain, vec!["old failure"]);
        // The ring wins once it has an entry.
        record_in(dir.path(), &entry(10, &["bougie", "sync"], &["new"]));
        assert_eq!(load(dir.path()).unwrap().argv[1], "sync");
    }

    #[test]
    fn format_epoch_renders_utc() {
        // 2026-07-09 06:12:34 UTC
        assert_eq!(format_epoch(1_783_577_554), "2026-07-09 06:12:34 UTC");
        assert_eq!(format_epoch(0), "1970-01-01 00:00:00 UTC");
    }
}
