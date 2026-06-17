//! Unified/git-diff parsing: split a (possibly multi-file) patch into
//! per-file units, expose each file's raw header paths, and `-p`-strip them.
//!
//! flickzeug already does the heavy lifting — its `patch_from_str` returns a
//! `Vec<Diff>`, one per file, and parses `diff --git` preambles, traditional
//! `--- / +++` headers, and pure rename/delete/add headers. We disable its
//! built-in `a/`/`b/` stripping (`strip_ab_prefix: false`) so we own *all*
//! path stripping through the `-p` level, matching GNU `patch`.
//!
//! This one parser serves three consumers: the `patches/` dir target
//! inference (Phase C), multi-file splitting, and per-file path routing.

use eyre::{Result, eyre};
use flickzeug::{Diff, ParserConfig, patch_from_str_with_config};

/// One file's worth of a (possibly multi-file) patch.
#[derive(Debug)]
pub struct FileDiff<'a> {
    /// The `--- ` path token, verbatim (incl. any `a/` prefix). `None` when
    /// the source side is `/dev/null` (a created file).
    pub old_path: Option<String>,
    /// The `+++ ` path token, verbatim (incl. any `b/` prefix). `None` when
    /// the target side is `/dev/null` (a deleted file).
    pub new_path: Option<String>,
    /// The kind of change this file diff represents.
    pub kind: ChangeKind,
    /// The parsed, applyable diff (borrows the source patch text).
    pub diff: Diff<'a, str>,
}

/// What a single [`FileDiff`] does to its target file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Modify an existing file (both sides present).
    Modify,
    /// Create a new file (`--- /dev/null`).
    Create,
    /// Delete an existing file (`+++ /dev/null`).
    Delete,
}

impl FileDiff<'_> {
    /// The header path that names the on-disk file to act on, *before* `-p`
    /// stripping: the new side for create/modify, the old side for delete.
    pub fn routed_path(&self) -> Option<&str> {
        match self.kind {
            ChangeKind::Create | ChangeKind::Modify => self.new_path.as_deref(),
            ChangeKind::Delete => self.old_path.as_deref(),
        }
    }
}

/// Parse raw patch text into its per-file diffs, preserving raw header paths.
pub fn split(text: &str) -> Result<Vec<FileDiff<'_>>> {
    let config = ParserConfig {
        // We strip `a/`/`b/` ourselves via `-p`, so leave the tokens raw.
        strip_ab_prefix: false,
        ..ParserConfig::default()
    };
    let diffs = patch_from_str_with_config(text, config)
        .map_err(|e| eyre!("failed to parse patch: {e}"))?;

    Ok(diffs
        .into_iter()
        .map(|diff| {
            let old_path = diff.original().map(str::to_string);
            let new_path = diff.modified().map(str::to_string);
            let kind = match (&old_path, &new_path) {
                (None, Some(_)) => ChangeKind::Create,
                (Some(_), None) => ChangeKind::Delete,
                _ => ChangeKind::Modify,
            };
            FileDiff {
                old_path,
                new_path,
                kind,
                diff,
            }
        })
        .collect())
}

/// Strip `n` leading path components (GNU `patch -pN`).
///
/// `-p0` keeps the whole path; `-p1` drops the first component (e.g. the
/// `a/` of a git diff); higher levels drop more. Returns `None` when the
/// path has `n` or fewer components (the level can't apply to this file).
/// Leading `./` segments and empty components (from `//`) are skipped like
/// GNU `patch` does, and do not count toward `n`.
pub fn strip_components(path: &str, n: usize) -> Option<String> {
    let mut stripped = 0usize;
    let mut rest = path;
    while stripped < n {
        // Drop any leading slashes / `./` noise without consuming a level.
        while let Some(r) = rest.strip_prefix('/').or_else(|| rest.strip_prefix("./")) {
            rest = r;
        }
        let slash = rest.find('/')?;
        rest = &rest[slash + 1..];
        stripped += 1;
    }
    // Clean any leftover leading slashes after the final strip.
    while let Some(r) = rest.strip_prefix('/').or_else(|| rest.strip_prefix("./")) {
        rest = r;
    }
    if rest.is_empty() { None } else { Some(rest.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_levels() {
        let p = "a/vendor/foo/src/Bar.php";
        assert_eq!(strip_components(p, 0).as_deref(), Some("a/vendor/foo/src/Bar.php"));
        assert_eq!(strip_components(p, 1).as_deref(), Some("vendor/foo/src/Bar.php"));
        assert_eq!(strip_components(p, 2).as_deref(), Some("foo/src/Bar.php"));
        assert_eq!(strip_components(p, 4).as_deref(), Some("Bar.php"));
        assert_eq!(strip_components(p, 5), None);
        assert_eq!(strip_components(p, 6), None);
    }

    #[test]
    fn strip_skips_dot_slash_and_double_slash() {
        assert_eq!(strip_components("./a/b/c", 1).as_deref(), Some("b/c"));
        assert_eq!(strip_components("a//b/c", 1).as_deref(), Some("b/c"));
    }

    #[test]
    fn split_single_file_git_diff() {
        let text = "\
diff --git a/src/Foo.php b/src/Foo.php
index 1111111..2222222 100644
--- a/src/Foo.php
+++ b/src/Foo.php
@@ -1,3 +1,3 @@
 line one
-line two
+line two patched
 line three
";
        let files = split(text).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].kind, ChangeKind::Modify);
        assert_eq!(files[0].old_path.as_deref(), Some("a/src/Foo.php"));
        assert_eq!(files[0].new_path.as_deref(), Some("b/src/Foo.php"));
        assert_eq!(files[0].routed_path(), Some("b/src/Foo.php"));
    }

    #[test]
    fn split_multi_file() {
        let text = "\
--- a/one.txt
+++ b/one.txt
@@ -1 +1 @@
-a
+b
--- a/two.txt
+++ b/two.txt
@@ -1 +1 @@
-c
+d
";
        let files = split(text).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path.as_deref(), Some("b/one.txt"));
        assert_eq!(files[1].new_path.as_deref(), Some("b/two.txt"));
    }

    #[test]
    fn split_create_and_delete() {
        let create = "\
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
";
        let files = split(create).unwrap();
        assert_eq!(files[0].kind, ChangeKind::Create);
        assert!(files[0].old_path.is_none());
        assert_eq!(files[0].routed_path(), Some("b/new.txt"));

        let delete = "\
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-hello
-world
";
        let files = split(delete).unwrap();
        assert_eq!(files[0].kind, ChangeKind::Delete);
        assert!(files[0].new_path.is_none());
        assert_eq!(files[0].routed_path(), Some("a/gone.txt"));
    }
}
