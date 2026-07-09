//! Apply a patch file to a tree on disk.
//!
//! A patch applies as a unit at a single `-p` level. For an explicit depth we
//! try just that level; otherwise we walk the cweagans fallback loop
//! (`-p1 -p0 -p2 -p4`) and take the first level at which *every* file in the
//! patch resolves and every hunk applies. Application is planned entirely in
//! memory first and only committed to disk once the whole patch succeeds, so a
//! patch never leaves a half-modified tree behind.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr as _, bail};
use flickzeug::{ApplyConfig, FuzzyConfig, apply_with_config};

use crate::diff::{self, ChangeKind, FileDiff, strip_components};
use crate::model::DepthSpec;
use crate::report::{ApplyReport, FileAction, FileOutcome};

/// Knobs for a single patch application.
#[derive(Debug, Clone)]
pub struct ApplyOptions {
    /// Strip-depth selection (`-pN` or the fallback loop).
    pub depth: DepthSpec,
    /// Fuzz factor handed to flickzeug (context lines it may ignore).
    pub max_fuzz: usize,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        // flickzeug's own default fuzz; GNU `patch` defaults to 2 as well.
        Self {
            depth: DepthSpec::Auto,
            max_fuzz: 2,
        }
    }
}

/// A single committed file operation, computed in the planning pass.
struct PlannedOp<'a> {
    abs_path: PathBuf,
    rel_path: String,
    /// `Some(content)` to write (create/modify); `None` to delete.
    new_content: Option<String>,
    file: &'a FileDiff<'a>,
}

/// Apply `patch_text` to the tree rooted at `base_dir`.
///
/// Returns the [`ApplyReport`] for the level that succeeded, or an error
/// describing the last failure if no candidate depth applied cleanly.
pub fn apply_patch_text(
    base_dir: &Path,
    patch_text: &str,
    opts: &ApplyOptions,
) -> Result<ApplyReport> {
    let files = diff::split(patch_text)?;
    if files.is_empty() {
        bail!("patch contains no file diffs");
    }

    let config = ApplyConfig {
        fuzzy_config: FuzzyConfig {
            max_fuzz: opts.max_fuzz,
            ignore_whitespace: false,
            ignore_case: false,
        },
        ..ApplyConfig::default()
    };

    let candidates = opts.depth.candidates();
    let mut last_err: Option<eyre::Report> = None;

    for depth in candidates {
        match plan_at_depth(base_dir, &files, depth, &config) {
            Ok((planned, mut report)) => {
                commit(&planned)?;
                report.files = planned
                    .iter()
                    .map(|op| FileOutcome {
                        path: op.rel_path.clone(),
                        action: action_for(op.file.kind),
                    })
                    .collect();
                report.depth_used = depth;
                return Ok(report);
            }
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err.unwrap_or_else(|| eyre::eyre!("patch did not apply at any depth")))
}

/// Attempt to plan the whole patch at one `-p` level. Any failure (a level
/// that strips a path away, a missing target file, a hunk that won't place)
/// returns `Err`, signalling the caller to try the next level.
fn plan_at_depth<'a>(
    base_dir: &Path,
    files: &'a [FileDiff<'a>],
    depth: usize,
    config: &ApplyConfig,
) -> Result<(Vec<PlannedOp<'a>>, ApplyReport)> {
    let mut ops = Vec::with_capacity(files.len());
    let mut report = ApplyReport {
        depth_used: depth,
        files: Vec::new(),
        lines_added: 0,
        lines_deleted: 0,
        hunks_applied: 0,
    };

    for file in files {
        let routed = file
            .routed_path()
            .ok_or_else(|| eyre::eyre!("file diff has no usable header path"))?;
        let rel = strip_components(routed, depth)
            .ok_or_else(|| eyre::eyre!("`-p{depth}` strips `{routed}` to nothing"))?;
        let abs_path = base_dir.join(&rel);

        let new_content = match file.kind {
            ChangeKind::Modify => {
                let base = fs::read_to_string(&abs_path).wrap_err_with(|| {
                    format!("cannot read `{}` for patching", abs_path.display())
                })?;
                let (content, stats) = apply_with_config(&base, &file.diff, config)
                    .map_err(|e| eyre::eyre!("hunk failed on `{rel}`: {e}"))?;
                report.lines_added += stats.lines_added;
                report.lines_deleted += stats.lines_deleted;
                report.hunks_applied += stats.hunks_applied;
                Some(content)
            }
            ChangeKind::Create => {
                let (content, stats) = apply_with_config("", &file.diff, config)
                    .map_err(|e| eyre::eyre!("cannot synthesize new file `{rel}`: {e}"))?;
                report.lines_added += stats.lines_added;
                report.hunks_applied += stats.hunks_applied;
                Some(content)
            }
            ChangeKind::Delete => {
                if !abs_path.exists() {
                    bail!("cannot delete `{}`: not found", abs_path.display());
                }
                None
            }
        };

        ops.push(PlannedOp {
            abs_path,
            rel_path: rel,
            new_content,
            file,
        });
    }

    Ok((ops, report))
}

/// Commit a fully-planned set of file operations to disk.
fn commit(ops: &[PlannedOp<'_>]) -> Result<()> {
    for op in ops {
        match &op.new_content {
            Some(content) => {
                if let Some(parent) = op.abs_path.parent() {
                    fs::create_dir_all(parent).wrap_err_with(|| {
                        format!("cannot create `{}`", parent.display())
                    })?;
                }
                atomic_write(&op.abs_path, content.as_bytes())?;
            }
            None => {
                fs::remove_file(&op.abs_path).wrap_err_with(|| {
                    format!("cannot delete `{}`", op.abs_path.display())
                })?;
            }
        }
    }
    Ok(())
}

/// Determine the report action from a change kind.
fn action_for(kind: ChangeKind) -> FileAction {
    match kind {
        ChangeKind::Modify => FileAction::Modified,
        ChangeKind::Create => FileAction::Created,
        ChangeKind::Delete => FileAction::Deleted,
    }
}

/// Write `bytes` to `path` atomically (temp file in the same dir + rename),
/// so a crash mid-write never leaves a truncated source file.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile_in(dir)?;
    tmp.write_all(bytes).wrap_err("write to temp file failed")?;
    tmp.flush().ok();
    let tmp_path = tmp.into_temp_path();
    tmp_path
        .persist(path)
        .wrap_err_with(|| format!("cannot finalize `{}`", path.display()))?;
    Ok(())
}

fn tempfile_in(dir: &Path) -> Result<tempfile::NamedTempFile> {
    tempfile::Builder::new()
        .prefix(".bougie-patch-")
        .tempfile_in(dir)
        .wrap_err_with(|| format!("cannot create temp file in `{}`", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    #[test]
    fn modify_at_p1() {
        let dir = tempdir().unwrap();
        write(dir.path(), "src/Foo.php", "line one\nline two\nline three\n");
        let patch = "\
--- a/src/Foo.php
+++ b/src/Foo.php
@@ -1,3 +1,3 @@
 line one
-line two
+line two patched
 line three
";
        let report = apply_patch_text(dir.path(), patch, &ApplyOptions::default()).unwrap();
        assert_eq!(report.depth_used, 1);
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].action, FileAction::Modified);
        let after = fs::read_to_string(dir.path().join("src/Foo.php")).unwrap();
        assert_eq!(after, "line one\nline two patched\nline three\n");
    }

    #[test]
    fn modify_p4_magento_style() {
        // A Magento-style patch with a deep `a/b/c/d/` prefix needs -p4.
        let dir = tempdir().unwrap();
        write(dir.path(), "Model/Foo.php", "alpha\nbeta\n");
        let patch = "\
--- a/vendor/magento/module/Model/Foo.php
+++ b/vendor/magento/module/Model/Foo.php
@@ -1,2 +1,2 @@
 alpha
-beta
+beta patched
";
        let report = apply_patch_text(dir.path(), patch, &ApplyOptions::default()).unwrap();
        assert_eq!(report.depth_used, 4);
        let after = fs::read_to_string(dir.path().join("Model/Foo.php")).unwrap();
        assert_eq!(after, "alpha\nbeta patched\n");
    }

    #[test]
    fn create_new_file() {
        let dir = tempdir().unwrap();
        let patch = "\
--- /dev/null
+++ b/added.txt
@@ -0,0 +1,2 @@
+hello
+world
";
        let report = apply_patch_text(dir.path(), patch, &ApplyOptions::default()).unwrap();
        assert_eq!(report.files[0].action, FileAction::Created);
        let after = fs::read_to_string(dir.path().join("added.txt")).unwrap();
        assert_eq!(after, "hello\nworld\n");
    }

    #[test]
    fn delete_file() {
        let dir = tempdir().unwrap();
        write(dir.path(), "gone.txt", "hello\nworld\n");
        let patch = "\
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-hello
-world
";
        let report = apply_patch_text(dir.path(), patch, &ApplyOptions::default()).unwrap();
        assert_eq!(report.files[0].action, FileAction::Deleted);
        assert!(!dir.path().join("gone.txt").exists());
    }

    #[test]
    fn multi_file_atomic_all_or_nothing() {
        let dir = tempdir().unwrap();
        write(dir.path(), "a.txt", "a\n");
        // b.txt intentionally absent → modify must fail and roll the whole
        // patch back (a.txt stays untouched).
        let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-a
+A
--- a/b.txt
+++ b/b.txt
@@ -1 +1 @@
-b
+B
";
        let opts = ApplyOptions {
            depth: DepthSpec::Fixed(1),
            ..ApplyOptions::default()
        };
        assert!(apply_patch_text(dir.path(), patch, &opts).is_err());
        // a.txt was never written because b.txt failed during planning.
        assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "a\n");
    }

    #[test]
    fn fuzzy_applies_with_context_drift() {
        // Context lines differ slightly from the patch; fuzzy matching should
        // still place the hunk.
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "f.txt",
            "header changed\nkeep\ntarget\nkeep2\nfooter changed\n",
        );
        let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1,5 +1,5 @@
 header
 keep
-target
+target patched
 keep2
 footer
";
        let report = apply_patch_text(dir.path(), patch, &ApplyOptions::default()).unwrap();
        let after = fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert!(after.contains("target patched"), "got: {after:?}");
        assert_eq!(report.files[0].action, FileAction::Modified);
    }
}
