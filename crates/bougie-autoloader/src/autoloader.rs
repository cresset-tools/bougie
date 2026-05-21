//! Live, in-memory autoloader model.
//!
//! `dump_autoload` is now a thin wrapper around `Autoloader::bootstrap`
//! followed by `Autoloader::emit`. The split lets a long-running
//! process (e.g. `bougie-server`) hold an `Autoloader` between
//! requests and apply single-file edits via `apply_changed_path` /
//! `apply_deleted_path` without re-walking the whole project. See
//! `INCREMENTAL_AUTOLOADER_PLAN.md`.
//!
//! What this module owns:
//!
//! - The fully-resolved classmap task list (same shape Composer's
//!   `AutoloadGenerator::dump` walks; see [`collect::build_classmap_tasks`]).
//! - Per-task per-file class lists (`BTreeMap<rel_path, Vec<class>>`):
//!   the patch flow needs each file individually addressable so an
//!   edit replaces just that file's contribution.
//! - The merged `class → path_expr` map used by both
//!   `vendor/composer/autoload_classmap.php` and the static-loader's
//!   `$classMap` array.
//! - The PSR-4 / PSR-0 / files entry lists and the prolog/header bits
//!   (`suffix`, `apcu_prefix`) — these are pure functions of
//!   `composer.lock` + `composer.json`, computed once at bootstrap.
//!   A patch only mutates `tasks[*].per_file` and `merged`.
//! - An [`AutoloadHeader`] capturing the inputs the bootstrap reduced
//!   to its current state. The server compares against a fresh
//!   request via [`Autoloader::header_matches`] to decide whether a
//!   live patch is enough or a full re-bootstrap is required.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::collect::{
    build_classmap_tasks, canonical, installed_versions_row, strip_leading_slash, task_path_expr,
    ClassmapEntry, Entry, FileEntry, Task,
};
use crate::emit;
use crate::installed;
use crate::lock::{self, RootManifest};
use crate::scan::{self, ExcludePatterns};
use crate::vendored;
use crate::{random_hex_chars, write_atomic, DumpError, DumpRequest};

/// Per-task live state. `task` is the immutable scan descriptor.
/// `per_file` maps a file's path (relative to `task.install_abs`) to
/// the ordered list of classes that file contributes to the classmap.
/// Files with zero classes after filtering are absent.
#[derive(Debug)]
struct TaskState {
    task: Task,
    per_file: BTreeMap<PathBuf, Vec<String>>,
}

/// Snapshot of the inputs that produced this autoloader's state.
/// Compared against a fresh [`DumpRequest`] to detect drift: a header
/// match means a live patch is sufficient; a mismatch means the
/// project needs a full re-bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoloadHeader {
    /// `composer.lock`'s `content-hash` at bootstrap time.
    pub lock_content_hash: String,
    /// Hash of the root manifest's autoload block (psr-4 / psr-0 /
    /// classmap / files / exclude-from-classmap). A user editing
    /// `composer.json` to add a new PSR-4 root invalidates this.
    pub autoload_config_hash: String,
    /// Flag bits that affect what gets emitted. `optimize` and
    /// `classmap_authoritative` change which tasks the build
    /// constructs; `no_dev` filters which packages contribute.
    pub flags: HeaderFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderFlags {
    pub optimize: bool,
    pub classmap_authoritative: bool,
    pub no_dev: bool,
    pub apcu_autoloader: bool,
}

/// One project's classmap model.
///
/// Construct via [`Autoloader::bootstrap`]; emit to disk via
/// [`Autoloader::emit`]. Between the two, the server can call
/// [`Autoloader::apply_changed_path`] / [`Autoloader::apply_deleted_path`]
/// to fold filesystem events into the merged classmap.
#[derive(Debug)]
pub struct Autoloader {
    project_root: PathBuf,
    tasks: Vec<TaskState>,
    exclude_default: ExcludePatterns,
    exclude_with_vendor: ExcludePatterns,
    /// Sorted by class name (BTreeMap iteration order) → ready for
    /// `emit::classmap` after a `.into_iter()` map.
    merged: BTreeMap<String, String>,
    psr4: Vec<Entry>,
    psr0: Vec<Entry>,
    files: Vec<FileEntry>,
    suffix: String,
    apcu_prefix: Option<String>,
    classmap_authoritative: bool,
    no_dev: bool,
    header: AutoloadHeader,
}

impl Autoloader {
    /// Read `composer.lock` + `composer.json`, build the classmap
    /// scan task list, drive the parallel scan, and produce a ready-
    /// to-emit `Autoloader`. Performs no I/O beyond reading source
    /// files; emit is a separate step.
    pub fn bootstrap(req: &DumpRequest<'_>) -> Result<Self, DumpError> {
        let lock = lock::read_lock(req.project_root)?;
        let manifest = lock::read_root_manifest(req.project_root)?;

        let suffix: String = req
            .autoloader_suffix
            .clone()
            .or_else(|| manifest.config.autoloader_suffix.clone())
            .unwrap_or_else(|| lock.content_hash.clone());

        let apcu_prefix: Option<String> = if req.apcu_autoloader {
            Some(
                req.apcu_prefix
                    .clone()
                    .unwrap_or_else(|| random_hex_chars(20)),
            )
        } else {
            None
        };

        let psr4 = crate::collect::psr4(&manifest, &lock, req.no_dev);
        let psr0 = crate::collect::psr0(&manifest, &lock, req.no_dev);
        let files = crate::collect::files(&manifest, &lock, req.no_dev);

        // `--classmap-authoritative` implies `--optimize` (Composer's
        // dump() does `if (classmapAuthoritative) $scanPsrPackages =
        // true`).
        let optimize = req.optimize || req.classmap_authoritative;

        let set = build_classmap_tasks(
            &manifest,
            &lock,
            req.no_dev,
            optimize,
            req.project_root,
        );
        let exclude_default = set.exclude_default;
        let exclude_with_vendor = set.exclude_with_vendor;

        // Parallel per-task scan capturing per-file class lists.
        let per_file_results: Vec<BTreeMap<PathBuf, Vec<String>>> = set
            .tasks
            .par_iter()
            .map(|task| {
                let exclude = if task.needs_vendor_exclude {
                    &exclude_with_vendor
                } else {
                    &exclude_default
                };
                scan::scan_per_file(&task.scan_root, &task.install_abs, &task.filter, exclude)
            })
            .collect();

        let tasks: Vec<TaskState> = set
            .tasks
            .into_iter()
            .zip(per_file_results.into_iter())
            .map(|(task, per_file)| TaskState { task, per_file })
            .collect();

        let merged = merge_classmap(&tasks);

        let header = AutoloadHeader {
            lock_content_hash: lock.content_hash.clone(),
            autoload_config_hash: autoload_config_hash(&manifest),
            flags: HeaderFlags {
                optimize,
                classmap_authoritative: req.classmap_authoritative,
                no_dev: req.no_dev,
                apcu_autoloader: req.apcu_autoloader,
            },
        };

        Ok(Self {
            project_root: req.project_root.to_path_buf(),
            tasks,
            exclude_default,
            exclude_with_vendor,
            merged,
            psr4,
            psr0,
            files,
            suffix,
            apcu_prefix,
            classmap_authoritative: req.classmap_authoritative,
            no_dev: req.no_dev,
            header,
        })
    }

    /// Write every `vendor/composer/autoload_*.php` file plus the
    /// runtime ClassLoader, InstalledVersions, LICENSE, and
    /// `installed.{json,php}`. Atomic per file (rename-based).
    pub fn emit(&self) -> Result<(), DumpError> {
        let composer_dir = self.project_root.join("vendor").join("composer");
        std::fs::create_dir_all(&composer_dir)?;

        let classmap = self.classmap_entries();

        write_atomic(
            &composer_dir.join("autoload_psr4.php"),
            emit::psr4(&self.psr4).as_bytes(),
        )?;
        write_atomic(
            &composer_dir.join("autoload_namespaces.php"),
            emit::psr0(&self.psr0).as_bytes(),
        )?;
        write_atomic(
            &composer_dir.join("autoload_classmap.php"),
            emit::classmap(&classmap).as_bytes(),
        )?;
        if !self.files.is_empty() {
            write_atomic(
                &composer_dir.join("autoload_files.php"),
                emit::files(&self.files).as_bytes(),
            )?;
        }

        write_atomic(
            &self.project_root.join("vendor").join("autoload.php"),
            emit::entry(&self.suffix).as_bytes(),
        )?;

        write_atomic(
            &composer_dir.join("autoload_real.php"),
            emit::real::emit(
                &self.suffix,
                !self.files.is_empty(),
                self.classmap_authoritative,
                self.apcu_prefix.as_deref(),
            )
            .as_bytes(),
        )?;

        write_atomic(
            &composer_dir.join("autoload_static.php"),
            emit::static_loader::emit(
                &self.suffix,
                &self.psr4,
                &self.psr0,
                &classmap,
                &self.files,
            )
            .as_bytes(),
        )?;

        vendored::write_runtime_files(&composer_dir, write_atomic)?;

        write_atomic(
            &composer_dir.join("installed.json"),
            installed::emit_installed_json(&self.project_root, self.no_dev)?.as_bytes(),
        )?;
        write_atomic(
            &composer_dir.join("installed.php"),
            installed::emit_installed_php(&self.project_root, self.no_dev)?.as_bytes(),
        )?;

        Ok(())
    }

    /// Re-scan one file and fold the result into the merged classmap.
    ///
    /// Returns `Ok(true)` iff `self.merged` actually changed (so the
    /// caller can skip emitting when an edit didn't move the
    /// classmap, e.g. a comment-only change). Returns `Ok(false)`
    /// when `abs_path` falls outside every task's `scan_root`, when
    /// it's excluded by `exclude-from-classmap`, when its extension
    /// isn't `.php` / `.inc`, or when the post-edit content
    /// produces the same class list as before.
    pub fn apply_changed_path(&mut self, abs_path: &Path) -> Result<bool, DumpError> {
        if !has_php_ext(abs_path) {
            return Ok(false);
        }
        let canon = canonical(abs_path.to_path_buf());
        let matching: Vec<usize> = self
            .tasks
            .iter()
            .enumerate()
            .filter_map(|(i, s)| canon.starts_with(&s.task.scan_root).then_some(i))
            .collect();
        if matching.is_empty() {
            return Ok(false);
        }

        let mut any_state_change = false;
        for i in matching {
            let exclude = if self.tasks[i].task.needs_vendor_exclude {
                &self.exclude_with_vendor
            } else {
                &self.exclude_default
            };
            let rel = canon
                .strip_prefix(&self.tasks[i].task.install_abs)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| canon.clone());

            let new_classes = scan::scan_one(&canon, &self.tasks[i].task.filter, exclude);
            let state = &mut self.tasks[i];
            match new_classes {
                Some(classes) => {
                    let prev = state.per_file.get(&rel);
                    if prev != Some(&classes) {
                        state.per_file.insert(rel, classes);
                        any_state_change = true;
                    }
                }
                None => {
                    if state.per_file.remove(&rel).is_some() {
                        any_state_change = true;
                    }
                }
            }
        }

        if !any_state_change {
            return Ok(false);
        }

        let new_merged = merge_classmap(&self.tasks);
        if new_merged == self.merged {
            return Ok(false);
        }
        self.merged = new_merged;
        Ok(true)
    }

    /// Drop a deleted file from every task's `per_file` map and
    /// re-merge. Returns `Ok(true)` iff `self.merged` changed.
    /// Idempotent: a delete for a file the autoloader never saw is
    /// a no-op.
    pub fn apply_deleted_path(&mut self, abs_path: &Path) -> Result<bool, DumpError> {
        let canon = canonical(abs_path.to_path_buf());
        let mut any_state_change = false;
        for state in &mut self.tasks {
            if !canon.starts_with(&state.task.scan_root) {
                continue;
            }
            let rel = canon
                .strip_prefix(&state.task.install_abs)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| canon.clone());
            if state.per_file.remove(&rel).is_some() {
                any_state_change = true;
            }
        }
        if !any_state_change {
            return Ok(false);
        }
        let new_merged = merge_classmap(&self.tasks);
        if new_merged == self.merged {
            return Ok(false);
        }
        self.merged = new_merged;
        Ok(true)
    }

    /// Inputs match the request that produced this autoloader.
    /// `false` means the manager should re-bootstrap.
    pub fn header_matches(&self, req: &DumpRequest<'_>) -> bool {
        let Ok(lock) = lock::read_lock(req.project_root) else {
            return false;
        };
        let Ok(manifest) = lock::read_root_manifest(req.project_root) else {
            return false;
        };
        let optimize = req.optimize || req.classmap_authoritative;
        let candidate = AutoloadHeader {
            lock_content_hash: lock.content_hash,
            autoload_config_hash: autoload_config_hash(&manifest),
            flags: HeaderFlags {
                optimize,
                classmap_authoritative: req.classmap_authoritative,
                no_dev: req.no_dev,
                apcu_autoloader: req.apcu_autoloader,
            },
        };
        candidate == self.header
    }

    /// Read-only access to the change-detection header. Exposed for
    /// tests and the manager's bookkeeping; the manager doesn't
    /// mutate it directly — re-bootstrap produces a new `Autoloader`.
    pub fn header(&self) -> &AutoloadHeader {
        &self.header
    }

    fn classmap_entries(&self) -> Vec<ClassmapEntry> {
        self.merged
            .iter()
            .map(|(class, path_expr)| ClassmapEntry {
                class: class.clone(),
                path_expr: path_expr.clone(),
            })
            .collect()
    }
}

/// Re-merge the per-task per-file class lists into one
/// `class → path_expr` map. First-seen wins across tasks (and across
/// files within a task; BTreeMap iteration is path-sorted, which
/// matches the walker's sort).
fn merge_classmap(tasks: &[TaskState]) -> BTreeMap<String, String> {
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for state in tasks {
        for (rel, classes) in &state.per_file {
            let path_expr = task_path_expr(&state.task, rel);
            for class in classes {
                merged
                    .entry(class.clone())
                    .or_insert_with(|| path_expr.clone());
            }
        }
    }
    let (iv_class, iv_path) = installed_versions_row();
    merged.entry(iv_class).or_insert(iv_path);
    merged
}

fn has_php_ext(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("php") || e.eq_ignore_ascii_case("inc"))
        .unwrap_or(false)
}

/// Hash of the root manifest's autoload block — covers psr-4, psr-0,
/// classmap, files, and exclude-from-classmap. A user editing
/// `composer.json` to add or remove an autoload directive flips this
/// hash, so [`Autoloader::header_matches`] reports drift and the
/// manager re-bootstraps. The lockfile-side autoload metadata is
/// already covered by `composer.lock`'s content-hash.
fn autoload_config_hash(manifest: &RootManifest) -> String {
    use md5::{Digest, Md5};
    use std::fmt::Write as _;

    let mut hasher = Md5::new();
    let push_pairs = |h: &mut Md5, label: &str, pairs: &[(String, Vec<String>)]| {
        h.update(label.as_bytes());
        h.update([0]);
        for (k, vs) in pairs {
            h.update(k.as_bytes());
            h.update([1]);
            for v in vs {
                h.update(v.as_bytes());
                h.update([2]);
            }
            h.update([3]);
        }
    };
    let push_list = |h: &mut Md5, label: &str, vs: &[String]| {
        h.update(label.as_bytes());
        h.update([0]);
        for v in vs {
            h.update(v.as_bytes());
            h.update([1]);
        }
    };
    push_pairs(&mut hasher, "psr4", &manifest.autoload.psr4);
    push_pairs(&mut hasher, "psr0", &manifest.autoload.psr0);
    push_list(&mut hasher, "classmap", &manifest.autoload.classmap);
    push_list(&mut hasher, "files", &manifest.autoload.files);
    push_list(
        &mut hasher,
        "exclude",
        &manifest.autoload.exclude_from_classmap,
    );

    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Directories the server's filesystem watcher needs to arm for live
/// patches: every root autoload `scan_root` plus the scan_roots of
/// any `dist.type: "path"` package in `composer.lock`. Vendor proper
/// (`zip` / `tar` dists) is intentionally excluded — those only
/// change via `composer install`, which rewrites `composer.lock` and
/// triggers a full re-bootstrap.
///
/// Reads only `composer.lock` + `composer.json` — sub-ms, so the
/// manager can call this before spawning the heavy bootstrap scan
/// and arm the watcher first (the ordering invariant in the plan's
/// "Save during Warming" test).
pub fn user_code_roots(req: &DumpRequest<'_>) -> Result<Vec<PathBuf>, DumpError> {
    let lock = lock::read_lock(req.project_root)?;
    let manifest = lock::read_root_manifest(req.project_root)?;

    let mut roots: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        let c = canonical(p);
        if !roots.iter().any(|r| r == &c) {
            roots.push(c);
        }
    };

    let project_root = req.project_root;
    for (_, dirs) in &manifest.autoload.psr4 {
        for d in dirs {
            push(project_root.join(strip_leading_slash(d)));
        }
    }
    for (_, dirs) in &manifest.autoload.psr0 {
        for d in dirs {
            push(project_root.join(strip_leading_slash(d)));
        }
    }
    for d in &manifest.autoload.classmap {
        push(project_root.join(strip_leading_slash(d)));
    }

    // Path-repo packages — Composer's `path` repository installs
    // typically symlink `vendor/<name>` to the source directory.
    // We canonicalize so the watcher arms the real source, not the
    // symlink (notify doesn't follow symlinks on read events).
    for pkg in lock.iter_packages(req.no_dev) {
        let is_path = pkg.dist.as_ref().is_some_and(|d| d.kind == "path");
        if !is_path {
            continue;
        }
        let install_abs = canonical(project_root.join(format!("vendor/{}", pkg.name)));
        let mut pushed = false;
        for (_, dirs) in &pkg.autoload.psr4 {
            for d in dirs {
                push(install_abs.join(strip_leading_slash(d)));
                pushed = true;
            }
        }
        for (_, dirs) in &pkg.autoload.psr0 {
            for d in dirs {
                push(install_abs.join(strip_leading_slash(d)));
                pushed = true;
            }
        }
        for d in &pkg.autoload.classmap {
            push(install_abs.join(strip_leading_slash(d)));
            pushed = true;
        }
        // Some path-repo packages declare no autoload at all (e.g.
        // a binary-only package). Watching `install_abs` lets a
        // subsequent autoload-config edit in that package's
        // composer.json be picked up by the lockfile watcher rather
        // than missed.
        if !pushed {
            push(install_abs);
        }
    }

    Ok(roots)
}

