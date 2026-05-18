//! Walk the lockfile + root manifest and produce the data shapes the
//! emitters consume.
//!
//! Order matters: Composer's autoload arrays are insertion-ordered,
//! so per-package entries land in lockfile order and root entries
//! come last (matching Composer's own pass order in
//! `AutoloadGenerator`).

use md5::{Digest, Md5};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::Path;

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
    let mut seen: BTreeMap<String, String> = BTreeMap::new();

    for pkg in lock.iter_packages(no_dev) {
        if pkg.autoload.classmap.is_empty() {
            continue;
        }
        let install_rel = format!("vendor/{}", pkg.name);
        let install_abs = project_root.join(&install_rel);
        for dir in &pkg.autoload.classmap {
            let scan_root = install_abs.join(strip_leading_slash(dir));
            for (class, file_abs) in scan::scan(&scan_root) {
                if seen.contains_key(&class) {
                    continue;
                }
                let rel = file_abs.strip_prefix(&install_abs).unwrap_or(&file_abs);
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let path_expr = format!("$vendorDir . '/{}/{rel_str}'", pkg.name);
                seen.insert(class, path_expr);
            }
        }
    }

    for dir in &root.autoload.classmap {
        let scan_root = project_root.join(strip_leading_slash(dir));
        for (class, file_abs) in scan::scan(&scan_root) {
            if seen.contains_key(&class) {
                continue;
            }
            let rel = file_abs.strip_prefix(project_root).unwrap_or(&file_abs);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let path_expr = format!("$baseDir . '/{rel_str}'");
            seen.insert(class, path_expr);
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
