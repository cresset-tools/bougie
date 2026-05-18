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
//! This module currently runs the pipeline sequentially. Parallelism
//! (`rayon::par_iter` over the file list, `rayon::scope` across
//! packages) lands in a follow-up PR per `AUTOLOADER_PLAN.md` Phase 2.

mod cleaner;
mod finder;
mod walker;

use std::path::{Path, PathBuf};

/// Scan a single classmap directory (or file). Returns
/// `(class_name, absolute_path)` pairs in the order they are
/// discovered. Deduplication and sort happen at a higher layer
/// (`collect::classmap`) so this module stays mechanical.
pub(crate) fn scan(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = vec![];
    for path in walker::enumerate(root, walker::DEFAULT_EXTENSIONS) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        for class in finder::find_classes(&bytes) {
            out.push((class, path.clone()));
        }
    }
    out
}
