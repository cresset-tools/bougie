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
mod walker;

use std::path::{Path, PathBuf};

use rayon::prelude::*;

pub(crate) use exclude::ExcludePatterns;
pub(crate) use filter::{NamespaceFilter, ScanWarning};

/// Result of scanning a single classmap (or PSR-*) directory.
/// `entries` are `(class_name, absolute_path)` pairs in walker order;
/// dedup and sort happen at a higher layer. `warnings` is the
/// PSR-noncompliance list produced when a file's classes were all
/// rejected by the namespace filter — empty for classmap-style scans.
pub(crate) struct ScanOutput {
    pub entries: Vec<(String, PathBuf)>,
    pub warnings: Vec<ScanWarning>,
}

/// Per-file shape used inside the parallel rayon stage of [`scan`].
/// Extracted only to keep clippy's `type_complexity` quiet.
type FileResult = (Vec<(String, PathBuf)>, Vec<ScanWarning>);

/// Scan a single classmap directory (or file). See [`ScanOutput`].
///
/// `filter` is [`NamespaceFilter::None`] for classmap-style scans
/// (every class kept) and `Psr4` / `Psr0` for the optimize-mode
/// PSR-* directory scans (class must match the namespace+path rule).
/// `exclude` is the precompiled exclude-from-classmap regex set;
/// files whose path matches it are skipped before they're read.
pub(crate) fn scan(
    root: &Path,
    filter: &NamespaceFilter,
    exclude: &ExcludePatterns,
) -> ScanOutput {
    let files = walker::enumerate(root, walker::DEFAULT_EXTENSIONS);
    let per_file: Vec<FileResult> = files
        .par_iter()
        .filter(|path| !exclude.matches(path))
        .map(|path| {
            let Ok(bytes) = std::fs::read(path) else {
                return (Vec::new(), Vec::new());
            };
            let classes = finder::find_classes(&bytes);
            let (kept, warnings) = filter::apply(filter, classes, path);
            let entries = kept
                .into_iter()
                .map(|c| (c, path.clone()))
                .collect::<Vec<_>>();
            (entries, warnings)
        })
        .collect();

    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for (e, w) in per_file {
        entries.extend(e);
        warnings.extend(w);
    }
    ScanOutput { entries, warnings }
}
