//! Classmap file-scan pipeline.
//!
//! Three stages, kept in separate modules so each is independently
//! testable and benchmarkable:
//! 1. [`walker`] — enumerate `.php` / `.inc` files under a classmap
//!    dir.
//! 2. [`cleaner`] — strip strings, comments, heredocs from each
//!    source file.
//! 3. [`finder`] — prefilter + extract class declarations from the
//!    cleaned source.
//!
//! File reads + clean + extract run in parallel via [`rayon`]. Output
//! order is preserved by `par_iter().flat_map_iter().collect()` so
//! per-file iteration order (and therefore the first-seen dedup at
//! `collect::classmap`) stays deterministic.

pub(crate) mod cleaner;
pub(crate) mod exclude;
pub(crate) mod filter;
pub(crate) mod finder;
pub(crate) mod walker;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

pub(crate) use exclude::ExcludePatterns;
pub(crate) use filter::{NamespaceFilter, ScanWarning};

/// Output of [`scan_per_file`]: per-file class lists keyed by the
/// file's path relative to `install_abs`, plus the PSR-noncompliance
/// warnings collected by the namespace filter (one per rejected class
/// in files where *every* class was rejected; empty for classmap-style
/// scans).
pub(crate) struct PerFileScan {
    pub per_file: BTreeMap<PathBuf, Vec<String>>,
    pub warnings: Vec<ScanWarning>,
}

/// Per-file shape used inside the parallel rayon stage of
/// [`scan_per_file`]. Pulled out only to keep clippy's
/// `type_complexity` quiet.
type FileResult = (Option<(PathBuf, Vec<String>)>, Vec<ScanWarning>);

/// Walk a task's `scan_root` and return per-file class lists keyed by
/// the file's path relative to `install_abs`. Used by
/// [`crate::Autoloader::bootstrap`] so each file's contribution is
/// individually addressable for incremental patches — when a file is
/// later edited, `apply_changed_path` re-scans just that one file
/// and replaces its entry without re-walking the whole task.
///
/// Iteration order of `BTreeMap<PathBuf, _>` is path-sorted, which
/// matches the walker's sort order: `walker::enumerate` sorts
/// absolute paths, and all files in a single task share the same
/// `install_abs` prefix, so relative-path sort is identical to
/// absolute-path sort. That equivalence is load-bearing — it
/// preserves first-seen-wins dedup behaviour now that the merge
/// walks per-file `BTreeMaps` instead of a flat per-task `Vec`.
///
/// Files whose filtered class list is empty are omitted: bootstrap
/// would not have recorded them, and `apply_changed_path` removes a
/// file's entry when its post-edit class list is empty. The filter's
/// per-file warnings (emitted only when *no* class in a file passed
/// the PSR filter) are still returned so the CLI can surface them.
#[tracing::instrument(skip_all, fields(root = %root.display()))]
pub(crate) fn scan_per_file(
    root: &Path,
    install_abs: &Path,
    filter: &NamespaceFilter,
    exclude: &ExcludePatterns,
) -> PerFileScan {
    let files = walker::enumerate(root, walker::DEFAULT_EXTENSIONS);
    let per_file_results: Vec<FileResult> = files
        .par_iter()
        .filter(|p| !exclude.matches(p))
        .map(|p| {
            let Ok(bytes) = std::fs::read(p) else {
                return (None, Vec::new());
            };
            let classes = finder::find_classes(&bytes);
            let (kept, warnings) = filter::apply(filter, classes, p);
            if kept.is_empty() {
                return (None, warnings);
            }
            let rel = p
                .strip_prefix(install_abs)
                .unwrap_or(p.as_path())
                .to_path_buf();
            (Some((rel, kept)), warnings)
        })
        .collect();

    let mut per_file: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    let mut warnings: Vec<ScanWarning> = Vec::new();
    for (entry, ws) in per_file_results {
        if let Some((rel, classes)) = entry {
            per_file.insert(rel, classes);
        }
        warnings.extend(ws);
    }
    PerFileScan { per_file, warnings }
}

/// Run the same cleaner+finder+filter pipeline a full-task scan
/// applies, but for a single file. Returns `None` when the file is
/// excluded, unreadable, or has zero classes after the namespace
/// filter — same callable shape as `scan_per_file`'s per-file
/// `filter_map` step. The per-file filter warnings are discarded:
/// `apply_changed_path` is a live-patch entry point that doesn't have
/// a surface to render them on (the CLI's `Generated …` footer is
/// only emitted by `dump_autoload`).
///
/// Callers (`Autoloader::apply_changed_path`) supply an absolute
/// path that already passed walker-style extension filtering.
pub(crate) fn scan_one(
    file_abs: &Path,
    filter: &NamespaceFilter,
    exclude: &ExcludePatterns,
) -> Option<Vec<String>> {
    if exclude.matches(file_abs) {
        return None;
    }
    let bytes = std::fs::read(file_abs).ok()?;
    let classes = finder::find_classes(&bytes);
    let (kept, _warnings) = filter::apply(filter, classes, file_abs);
    if kept.is_empty() { None } else { Some(kept) }
}
