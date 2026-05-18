//! Walk the lockfile + root manifest and produce the data shapes the
//! emitters consume.
//!
//! Order matters: Composer's autoload arrays are insertion-ordered,
//! so per-package entries land in lockfile order and root entries
//! come last (matching Composer's own pass order in
//! `AutoloadGenerator`).

use md5::{Digest, Md5};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use crate::lock::{LockFile, RootManifest};
use crate::scan;

/// One PSR-4 or PSR-0 prefix and its install-path-prefixed dirs.
pub(crate) struct Entry {
    pub prefix: String,
    pub paths: Vec<String>,
}

pub(crate) fn psr4(root: &RootManifest, lock: &LockFile, no_dev: bool) -> Vec<Entry> {
    let mut out = vec![];
    for pkg in lock.iter_packages(no_dev) {
        let install_path = format!("vendor/{}", pkg.name);
        for (prefix, dirs) in &pkg.autoload.psr4 {
            let paths = dirs
                .iter()
                .map(|d| format!("$vendorDir . '/{}'", join_rel(&install_path, d)))
                .collect();
            out.push(Entry {
                prefix: prefix.clone(),
                paths,
            });
        }
    }
    for (prefix, dirs) in &root.autoload.psr4 {
        let paths = dirs
            .iter()
            .map(|d| format!("$baseDir . '/{}'", strip_leading_slash(d)))
            .collect();
        out.push(Entry {
            prefix: prefix.clone(),
            paths,
        });
    }
    out
}

pub(crate) fn psr0(root: &RootManifest, lock: &LockFile, no_dev: bool) -> Vec<Entry> {
    let mut out = vec![];
    for pkg in lock.iter_packages(no_dev) {
        let install_path = format!("vendor/{}", pkg.name);
        for (prefix, dirs) in &pkg.autoload.psr0 {
            let paths = dirs
                .iter()
                .map(|d| format!("$vendorDir . '/{}'", join_rel(&install_path, d)))
                .collect();
            out.push(Entry {
                prefix: prefix.clone(),
                paths,
            });
        }
    }
    for (prefix, dirs) in &root.autoload.psr0 {
        let paths = dirs
            .iter()
            .map(|d| format!("$baseDir . '/{}'", strip_leading_slash(d)))
            .collect();
        out.push(Entry {
            prefix: prefix.clone(),
            paths,
        });
    }
    out
}

pub(crate) struct FileEntry {
    pub identifier: String,
    pub path_expr: String,
}

pub(crate) fn files(root: &RootManifest, lock: &LockFile, no_dev: bool) -> Vec<FileEntry> {
    let mut out = vec![];
    for pkg in lock.iter_packages(no_dev) {
        let install_path = format!("vendor/{}", pkg.name);
        for f in &pkg.autoload.files {
            out.push(FileEntry {
                identifier: file_identifier(&pkg.name, f),
                path_expr: format!("$vendorDir . '/{}'", join_rel(&install_path, f)),
            });
        }
    }
    for f in &root.autoload.files {
        out.push(FileEntry {
            identifier: file_identifier("__root__", f),
            path_expr: format!("$baseDir . '/{}'", strip_leading_slash(f)),
        });
    }
    out
}

/// One classmap row: `'<class>' => <path expression>`.
pub(crate) struct ClassmapEntry {
    pub class: String,
    pub path_expr: String,
}

/// Walk every `autoload.classmap` directory across packages + root,
/// run the scan pipeline, and produce a deduped, alphabetically
/// sorted entry list ready for emit. The synthetic
/// `Composer\InstalledVersions` entry is always included — Composer
/// emits it unconditionally, even when no user classes are found.
///
/// Dedup: first occurrence wins. Packages are walked in lockfile
/// order (prod first, then dev when not skipped), root entries last
/// — same as the PSR-4/PSR-0 collectors.
pub(crate) fn classmap(
    root: &RootManifest,
    lock: &LockFile,
    no_dev: bool,
    project_root: &Path,
) -> Vec<ClassmapEntry> {
    // Flatten (package, dir) pairs in lockfile order, then a final
    // pass for root entries. Each `Task` owns the scan root and the
    // closure that turns scan output into a PHP path expression.
    enum Origin<'a> {
        Package(&'a str), // package name → `$vendorDir . '/<name>/...'`
        Root,             // root entry → `$baseDir . '/...'`
    }
    struct Task<'a> {
        origin: Origin<'a>,
        install_abs: PathBuf, // path the scanner walks
        scan_root: PathBuf,   // absolute path passed to scan::scan
    }

    let mut tasks: Vec<Task<'_>> = Vec::new();
    for pkg in lock.iter_packages(no_dev) {
        if pkg.autoload.classmap.is_empty() {
            continue;
        }
        let install_abs = project_root.join(format!("vendor/{}", pkg.name));
        for dir in &pkg.autoload.classmap {
            tasks.push(Task {
                origin: Origin::Package(&pkg.name),
                scan_root: install_abs.join(strip_leading_slash(dir)),
                install_abs: install_abs.clone(),
            });
        }
    }
    for dir in &root.autoload.classmap {
        tasks.push(Task {
            origin: Origin::Root,
            scan_root: project_root.join(strip_leading_slash(dir)),
            install_abs: project_root.to_path_buf(),
        });
    }

    // Parallel scan across (package, dir) pairs — rayon's `collect`
    // preserves source order so the sequential merge below sees
    // results in the same order as the lockfile + root iteration.
    let per_task: Vec<Vec<(String, String)>> = tasks
        .par_iter()
        .map(|task| {
            scan::scan(&task.scan_root)
                .into_iter()
                .map(|(class, file_abs)| {
                    let rel = file_abs
                        .strip_prefix(&task.install_abs)
                        .unwrap_or(&file_abs);
                    let rel_str = rel.to_string_lossy().replace('\\', "/");
                    let path_expr = match task.origin {
                        Origin::Package(name) => format!("$vendorDir . '/{name}/{rel_str}'"),
                        Origin::Root => format!("$baseDir . '/{rel_str}'"),
                    };
                    (class, path_expr)
                })
                .collect()
        })
        .collect();

    // Sequential merge: first-seen wins across tasks, in iteration
    // order. The BTreeMap also sorts the final output alphabetically.
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for results in per_task {
        for (class, path_expr) in results {
            seen.entry(class).or_insert(path_expr);
        }
    }

    // Composer always emits the InstalledVersions row. The fixture's
    // expected output proves this even for projects with zero
    // user-supplied classes.
    seen.entry("Composer\\InstalledVersions".to_string())
        .or_insert_with(|| "$vendorDir . '/composer/InstalledVersions.php'".to_string());

    seen.into_iter()
        .map(|(class, path_expr)| ClassmapEntry { class, path_expr })
        .collect()
}

/// Composer's `AutoloadGenerator::getFileIdentifier`:
/// `md5(package_name + ':' + path)`. Uses RustCrypto's `md-5` — the
/// same MD5 implementation `bougie-composer` already depends on for
/// `composer.lock`'s content-hash, so we don't pull a second MD5
/// crate into the tree.
fn file_identifier(package_name: &str, path: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(format!("{package_name}:{path}"));
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Composer normalizes `psr-4`/`psr-0` paths by stripping leading `./`
/// and trailing `/`. `join_rel` builds the literal that appears in PHP
/// source: `<install_path>/<dir>` minus the trailing slash (Composer
/// omits it). Empty `dir` collapses to just the install path.
fn join_rel(install_path: &str, dir: &str) -> String {
    let trimmed = strip_leading_slash(dir).trim_end_matches('/');
    let pkg_part = install_path.strip_prefix("vendor/").unwrap_or(install_path);
    if trimmed.is_empty() {
        pkg_part.to_string()
    } else {
        format!("{pkg_part}/{trimmed}")
    }
}

fn strip_leading_slash(s: &str) -> &str {
    s.strip_prefix('/').unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_identifier_matches_composer() {
        // Cross-checked via `php -r 'echo md5("acme/helpers:functions.php");'`
        // — the value that appears in tests/fixtures/files-single/expected/
        // vendor/composer/autoload_files.php.
        assert_eq!(
            file_identifier("acme/helpers", "functions.php"),
            "15a74e8c7f50af51efa9794609612b23"
        );
    }
}
