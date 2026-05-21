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
pub(crate) use filter::NamespaceFilter;

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
/// walks per-file BTreeMaps instead of a flat per-task `Vec`.
///
/// Files whose filtered class list is empty are omitted: bootstrap
/// would not have recorded them, and `apply_changed_path` removes a
/// file's entry when its post-edit class list is empty.
pub(crate) fn scan_per_file(
    root: &Path,
    install_abs: &Path,
    filter: &NamespaceFilter,
    exclude: &ExcludePatterns,
) -> BTreeMap<PathBuf, Vec<String>> {
    let files = walker::enumerate(root, walker::DEFAULT_EXTENSIONS);
    let pairs: Vec<(PathBuf, Vec<String>)> = files
        .par_iter()
        .filter(|p| !exclude.matches(p))
        .filter_map(|p| {
            let bytes = std::fs::read(p).ok()?;
            let classes = finder::find_classes(&bytes);
            let kept = filter::apply(filter, classes, p);
            if kept.is_empty() {
                return None;
            }
            let rel = p
                .strip_prefix(install_abs)
                .unwrap_or(p.as_path())
                .to_path_buf();
            Some((rel, kept))
        })
        .collect();
    pairs.into_iter().collect()
}

/// Run the same cleaner+finder+filter pipeline a full-task scan
/// applies, but for a single file. Returns `None` when the file is
/// excluded, unreadable, or has zero classes after the namespace
/// filter — same callable shape as `scan_per_file`'s per-file
/// `filter_map` step.
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
    let kept = filter::apply(filter, classes, file_abs);
    if kept.is_empty() { None } else { Some(kept) }
}
