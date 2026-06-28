//! Author a patch from an edited package tree — the inverse of [`apply`].
//!
//! Given a package's pristine (originally-installed) file set and its
//! hand-edited file set, emit a git-style unified diff whose header paths are
//! `a/<prefix>/<rel>` / `b/<prefix>/<rel>`. With an empty prefix the paths are
//! package-relative (apply at `-p1`); with the install dir as the prefix they
//! are project-relative (e.g. `a/vendor/acme/foo/src/Foo.php`), which lets
//! bougie's zero-config `patches/` directory infer the target package and `-p`
//! depth. Either way the patch round-trips back through the applier onto a
//! fresh pristine extraction — see the round-trip tests below.
//!
//! This module is pure and in-memory: the host walks the two trees and hands
//! the per-file byte contents in. Binary (non-UTF-8) files can't be represented
//! in a text patch, so they are reported in [`MakeOutcome::binary_skipped`]
//! rather than silently dropped.
//!
//! [`apply`]: crate::apply

use similar::TextDiff;

/// One file's contents on each side, keyed by its package-relative path.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Path relative to the package root, `/`-separated.
    pub path: String,
    /// Pristine (originally-installed) bytes, or `None` if newly created.
    pub pristine: Option<Vec<u8>>,
    /// Edited bytes, or `None` if the file was deleted.
    pub edited: Option<Vec<u8>>,
}

/// The result of [`make_patch`].
#[derive(Debug, Default)]
pub struct MakeOutcome {
    /// The concatenated unified diff (empty when nothing changed).
    pub patch_text: String,
    /// Modified files (package-relative paths), sorted.
    pub modified: Vec<String>,
    /// Created files (present only in the edited tree).
    pub created: Vec<String>,
    /// Deleted files (present only in the pristine tree).
    pub deleted: Vec<String>,
    /// Files skipped because at least one side is binary (non-UTF-8) and so
    /// cannot be expressed in a text patch.
    pub binary_skipped: Vec<String>,
}

impl MakeOutcome {
    /// Total number of files represented in the generated patch.
    pub fn changed_count(&self) -> usize {
        self.modified.len() + self.created.len() + self.deleted.len()
    }
}

#[derive(Clone, Copy)]
enum Side {
    Modify,
    Create,
    Delete,
}

/// Build a unified diff over `files`. Header paths are prefixed with
/// `path_prefix` (use `""` for package-relative, or the install dir for
/// project-relative). Input order is irrelevant — entries are sorted by path so
/// the output is deterministic. The `modified`/`created`/`deleted` lists stay
/// `path_prefix`-free (the package-relative path of each file).
pub fn make_patch(files: &[FileEntry], path_prefix: &str) -> MakeOutcome {
    let mut entries: Vec<&FileEntry> = files.iter().collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    let mut out = MakeOutcome::default();
    for f in entries {
        match (&f.pristine, &f.edited) {
            (Some(p), Some(e)) => {
                if p == e {
                    continue; // unchanged
                }
                match (std::str::from_utf8(p), std::str::from_utf8(e)) {
                    (Ok(po), Ok(eo)) => {
                        out.patch_text
                            .push_str(&file_diff(path_prefix, &f.path, po, eo, Side::Modify));
                        out.modified.push(f.path.clone());
                    }
                    _ => out.binary_skipped.push(f.path.clone()),
                }
            }
            (None, Some(e)) => match std::str::from_utf8(e) {
                Ok(eo) => {
                    out.patch_text
                        .push_str(&file_diff(path_prefix, &f.path, "", eo, Side::Create));
                    out.created.push(f.path.clone());
                }
                Err(_) => out.binary_skipped.push(f.path.clone()),
            },
            (Some(p), None) => match std::str::from_utf8(p) {
                Ok(po) => {
                    out.patch_text
                        .push_str(&file_diff(path_prefix, &f.path, po, "", Side::Delete));
                    out.deleted.push(f.path.clone());
                }
                Err(_) => out.binary_skipped.push(f.path.clone()),
            },
            (None, None) => {}
        }
    }
    out
}

/// One file's unified diff with git-style `a/`/`b/` (or `/dev/null`) headers.
/// `similar` only emits the header when there is at least one hunk, so an
/// all-context diff yields the empty string.
fn file_diff(prefix: &str, path: &str, old: &str, new: &str, side: Side) -> String {
    let p = if prefix.is_empty() {
        path.to_string()
    } else {
        format!("{}/{path}", prefix.trim_end_matches('/'))
    };
    let (a, b) = match side {
        Side::Create => ("/dev/null".to_string(), format!("b/{p}")),
        Side::Delete => (format!("a/{p}"), "/dev/null".to_string()),
        Side::Modify => (format!("a/{p}"), format!("b/{p}")),
    };
    let diff = TextDiff::from_lines(old, new);
    let mut ud = diff.unified_diff();
    ud.context_radius(3);
    ud.header(&a, &b);
    ud.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::{ApplyOptions, apply_patch_text};
    use std::fs;
    use tempfile::tempdir;

    fn entry(path: &str, pristine: Option<&str>, edited: Option<&str>) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            pristine: pristine.map(|s| s.as_bytes().to_vec()),
            edited: edited.map(|s| s.as_bytes().to_vec()),
        }
    }

    fn entry_bytes(path: &str, pristine: Option<&[u8]>, edited: Option<&[u8]>) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            pristine: pristine.map(<[u8]>::to_vec),
            edited: edited.map(<[u8]>::to_vec),
        }
    }

    /// Materialize the pristine side into a scratch tree, apply the generated
    /// patch at its default (fallback) depth, and assert the result equals the
    /// edited side — the property that makes a generated patch trustworthy. The
    /// scratch tree is laid out package-relative; with a non-empty `prefix` the
    /// applier's `-p` fallback strips the prefix back off, so both layouts must
    /// round-trip.
    fn assert_roundtrip_with(prefix: &str, files: &[FileEntry]) {
        let out = make_patch(files, prefix);
        let dir = tempdir().unwrap();
        for f in files {
            if let Some(p) = &f.pristine {
                let abs = dir.path().join(&f.path);
                fs::create_dir_all(abs.parent().unwrap()).unwrap();
                fs::write(abs, p).unwrap();
            }
        }
        if !out.patch_text.is_empty() {
            apply_patch_text(dir.path(), &out.patch_text, &ApplyOptions::default())
                .unwrap_or_else(|e| panic!("generated patch failed to apply:\n{}\n{e}", out.patch_text));
        }
        for f in files {
            if out.binary_skipped.contains(&f.path) {
                continue;
            }
            let abs = dir.path().join(&f.path);
            match &f.edited {
                Some(e) => assert_eq!(fs::read(&abs).unwrap(), *e, "content mismatch for {}", f.path),
                None => assert!(!abs.exists(), "{} should have been deleted", f.path),
            }
        }
    }

    fn assert_roundtrip(files: &[FileEntry]) {
        assert_roundtrip_with("", files);
    }

    #[test]
    fn modify_create_delete_roundtrip() {
        assert_roundtrip(&[
            entry("src/Foo.php", Some("a\nb\nc\n"), Some("a\nB\nc\n")),
            entry("src/New.php", None, Some("<?php\nnew\n")),
            entry("legacy.txt", Some("gone\n"), None),
            entry("unchanged.txt", Some("same\n"), Some("same\n")),
        ]);
    }

    #[test]
    fn project_relative_prefix_roundtrips() {
        // Headers become `a/vendor/acme/foo/...`; the applier's -p fallback
        // (…-p4 here) strips the prefix back off and applies package-relative.
        assert_roundtrip_with(
            "vendor/acme/foo",
            &[
                entry("src/Foo.php", Some("a\nb\nc\n"), Some("a\nB\nc\n")),
                entry("src/New.php", None, Some("<?php\nnew\n")),
                entry("README.md", Some("gone\n"), None),
            ],
        );
        // Headers carry the install prefix.
        let out = make_patch(&[entry("src/Foo.php", Some("a\n"), Some("b\n"))], "vendor/acme/foo");
        assert!(out.patch_text.contains("--- a/vendor/acme/foo/src/Foo.php"), "{}", out.patch_text);
        assert!(out.patch_text.contains("+++ b/vendor/acme/foo/src/Foo.php"), "{}", out.patch_text);
        // …but the reported file list stays package-relative.
        assert_eq!(out.modified, ["src/Foo.php"]);
    }

    #[test]
    fn no_trailing_newline_roundtrip() {
        assert_roundtrip(&[entry("a.txt", Some("one\ntwo"), Some("one\nTWO"))]);
        assert_roundtrip(&[entry("b.txt", Some("one\ntwo\n"), Some("one\ntwo"))]);
    }

    #[test]
    fn classifies_changes() {
        let out = make_patch(
            &[
                entry("m", Some("a\n"), Some("b\n")),
                entry("c", None, Some("x\n")),
                entry("d", Some("y\n"), None),
                entry("u", Some("z\n"), Some("z\n")),
            ],
            "",
        );
        assert_eq!(out.modified, ["m"]);
        assert_eq!(out.created, ["c"]);
        assert_eq!(out.deleted, ["d"]);
        assert_eq!(out.changed_count(), 3);
        assert!(out.binary_skipped.is_empty());
        // The header paths must be git-style so the patch applies at -p1.
        assert!(out.patch_text.contains("--- a/m"));
        assert!(out.patch_text.contains("+++ b/m"));
        assert!(out.patch_text.contains("--- /dev/null"));
    }

    #[test]
    fn binary_files_are_skipped_not_emitted() {
        // 0x9f is not valid UTF-8.
        let out = make_patch(
            &[entry_bytes("img.bin", Some(&[0, 159, 146, 150]), Some(&[1, 2, 3]))],
            "",
        );
        assert!(out.patch_text.is_empty());
        assert_eq!(out.binary_skipped, ["img.bin"]);
        assert_eq!(out.changed_count(), 0);
    }

    #[test]
    fn empty_when_identical() {
        let out = make_patch(&[entry("a", Some("x\n"), Some("x\n"))], "");
        assert!(out.patch_text.is_empty());
        assert_eq!(out.changed_count(), 0);
    }
}
