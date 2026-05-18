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
pub(crate) mod finder;
mod walker;

use std::path::{Path, PathBuf};

use rayon::prelude::*;

/// Scan a single classmap directory (or file). Returns
/// `(class_name, absolute_path)` pairs in walker order. Deduplication
/// and sort happen at a higher layer (`collect::classmap`) so this
/// module stays mechanical.
pub(crate) fn scan(root: &Path) -> Vec<(String, PathBuf)> {
    let files = walker::enumerate(root, walker::DEFAULT_EXTENSIONS);
    files
        .par_iter()
        .flat_map_iter(|path| {
            let Ok(bytes) = std::fs::read(path) else {
                return Vec::new().into_iter();
            };
            finder::find_classes(&bytes)
                .into_iter()
                .map(|c| (c, path.clone()))
                .collect::<Vec<_>>()
                .into_iter()
        })
        .collect()
}
