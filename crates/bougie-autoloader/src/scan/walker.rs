//! Enumerate PHP source files under a classmap directory entry.
//!
//! Composer accepts a classmap entry as either a directory (recursive
//! scan) or a single file. Default extensions are `php` and `inc`.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

pub(crate) const DEFAULT_EXTENSIONS: &[&str] = &["php", "inc"];

pub(crate) fn enumerate(root: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    if root.is_file() {
        return if has_ext(root, extensions) {
            vec![root.to_path_buf()]
        } else {
            vec![]
        };
    }
    if !root.is_dir() {
        return vec![];
    }
    let mut out: Vec<PathBuf> = WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && has_ext(e.path(), extensions))
        .map(walkdir::DirEntry::into_path)
        .collect();
    // Sort for deterministic processing order. The final classmap is
    // sorted by class name before emit, but per-file iteration order
    // still matters for ambiguity warnings (which class wins when two
    // files declare the same name).
    out.sort();
    out
}

fn has_ext(p: &Path, exts: &[&str]) -> bool {
    let Some(ext) = p.extension().and_then(|s| s.to_str()) else {
        return false;
    };
    let lower = ext.to_ascii_lowercase();
    exts.iter().any(|e| *e == lower)
}
