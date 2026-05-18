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
use crate::scan::{self, ExcludePatterns, NamespaceFilter};

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
    krsort_entries(&mut out);
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
    krsort_entries(&mut out);
    out
}

/// PHP's `krsort` semantics: sort by key (here the namespace prefix)
/// in descending lex order. Composer applies this to the aggregated
/// psr-4/psr-0 maps before emit so the more-specific namespaces hit
/// the runtime ClassLoader first. Stable so per-package insertion
/// order is preserved within a prefix bucket.
fn krsort_entries(out: &mut Vec<Entry>) {
    out.sort_by(|a, b| b.prefix.cmp(&a.prefix));
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
/// optionally walk `autoload.psr-4` and `autoload.psr-0` directories
/// when `optimize` is set, run the scan pipeline, and produce a
/// deduped, alphabetically sorted entry list ready for emit. The
/// synthetic `Composer\InstalledVersions` entry is always included —
/// Composer emits it unconditionally, even when no user classes are
/// found.
///
/// Dedup: first occurrence wins. Packages are walked in lockfile
/// order (prod first, then dev when not skipped), root entries last
/// — same as the PSR-4/PSR-0 collectors. Within optimize mode the
/// classmap dirs are scanned first, then PSR-* dirs, mirroring the
/// order in `AutoloadGenerator::dump`.
#[allow(clippy::too_many_lines)] // task-list construction is naturally long and benefits from staying inline
pub(crate) fn classmap(
    root: &RootManifest,
    lock: &LockFile,
    no_dev: bool,
    optimize: bool,
    project_root: &Path,
) -> Vec<ClassmapEntry> {
    // Flatten scan tasks across packages + root. Each task owns its
    // scan root, the install path used to derive the emit-time
    // relative path, the namespace filter to apply to discovered
    // classes (None for classmap scans, Psr4/Psr0 for optimize-mode
    // PSR-* scans), and the origin (package vs root) that picks
    // between `$vendorDir` and `$baseDir`.
    enum Origin<'a> {
        Package(&'a str),
        Root,
    }
    struct Task<'a> {
        origin: Origin<'a>,
        install_abs: PathBuf,
        scan_root: PathBuf,
        filter: NamespaceFilter,
    }

    // Aggregate exclude-from-classmap patterns across packages + root.
    // Compilation needs each pattern's source install path so that
    // realpath() (canonicalize) resolves to the right absolute
    // directory; with that, all alternatives OR into one regex.
    //
    // Canonicalize install paths here too. On macOS `/var/folders/...`
    // is a symlink to `/private/var/folders/...`; the exclude regex
    // (compiled from canonicalize'd install + pattern) would otherwise
    // be anchored at one form while strip_prefix on file_abs uses the
    // other. Same trap on any platform where the project root sits
    // behind a symlink. We canonicalize once at this boundary so every
    // path that flows downstream (install_abs, scan_root, exclude
    // anchors, file_abs from walkdir) is in the same form.
    let mut exclude_patterns: Vec<(PathBuf, String)> = Vec::new();
    for pkg in lock.iter_packages(no_dev) {
        if pkg.autoload.exclude_from_classmap.is_empty() {
            continue;
        }
        let install_abs = canonical(project_root.join(format!("vendor/{}", pkg.name)));
        for raw in &pkg.autoload.exclude_from_classmap {
            exclude_patterns.push((install_abs.clone(), raw.clone()));
        }
    }
    for raw in &root.autoload.exclude_from_classmap {
        exclude_patterns.push((canonical(project_root.to_path_buf()), raw.clone()));
    }
    let exclude = ExcludePatterns::build(&exclude_patterns);

    let mut tasks: Vec<Task<'_>> = Vec::new();

    // Classmap dirs first — matches Composer's dump() order
    // (classmap pass, then optionally PSR-* pass).
    for pkg in lock.iter_packages(no_dev) {
        if pkg.autoload.classmap.is_empty() {
            continue;
        }
        let install_abs = canonical(project_root.join(format!("vendor/{}", pkg.name)));
        for dir in &pkg.autoload.classmap {
            tasks.push(Task {
                origin: Origin::Package(&pkg.name),
                scan_root: canonical(install_abs.join(strip_leading_slash(dir))),
                install_abs: install_abs.clone(),
                filter: NamespaceFilter::None,
            });
        }
    }
    for dir in &root.autoload.classmap {
        tasks.push(Task {
            origin: Origin::Root,
            scan_root: canonical(project_root.join(strip_leading_slash(dir))),
            install_abs: canonical(project_root.to_path_buf()),
            filter: NamespaceFilter::None,
        });
    }

    if optimize {
        // Composer's `dump()` buckets all PSR-* entries by namespace
        // and then runs `krsort` on the bucket keys — so more-
        // specific namespaces scan first, which on overlapping bases
        // controls which mapping claims a class via first-seen dedup.
        //
        // We collect candidate tasks tagged with their namespace,
        // then sort by namespace descending with a stable sort so
        // PSR-4 stays before PSR-0 within a namespace bucket (mirrors
        // Composer's `foreach (['psr-4', 'psr-0'] ...)` outer-loop
        // order). Cross-package order within a namespace bucket is
        // lockfile order — Composer's reverse-sortPackageMap order
        // is topological + root-first; matching it exactly is a
        // separate gap.
        let mut psr_tasks: Vec<(String, Task<'_>)> = Vec::new();
        for pkg in lock.iter_packages(no_dev) {
            let install_abs = canonical(project_root.join(format!("vendor/{}", pkg.name)));
            for (ns, dirs) in &pkg.autoload.psr4 {
                for dir in dirs {
                    let scan_root = canonical(install_abs.join(strip_leading_slash(dir)));
                    psr_tasks.push((
                        ns.clone(),
                        Task {
                            origin: Origin::Package(&pkg.name),
                            scan_root: scan_root.clone(),
                            install_abs: install_abs.clone(),
                            filter: NamespaceFilter::Psr4 {
                                namespace: ns.clone(),
                                base: scan_root,
                            },
                        },
                    ));
                }
            }
            for (ns, dirs) in &pkg.autoload.psr0 {
                for dir in dirs {
                    let scan_root = canonical(install_abs.join(strip_leading_slash(dir)));
                    psr_tasks.push((
                        ns.clone(),
                        Task {
                            origin: Origin::Package(&pkg.name),
                            scan_root: scan_root.clone(),
                            install_abs: install_abs.clone(),
                            filter: NamespaceFilter::Psr0 {
                                namespace: ns.clone(),
                                base: scan_root,
                            },
                        },
                    ));
                }
            }
        }
        for (ns, dirs) in &root.autoload.psr4 {
            for dir in dirs {
                let scan_root = canonical(project_root.join(strip_leading_slash(dir)));
                psr_tasks.push((
                    ns.clone(),
                    Task {
                        origin: Origin::Root,
                        scan_root: scan_root.clone(),
                        install_abs: canonical(project_root.to_path_buf()),
                        filter: NamespaceFilter::Psr4 {
                            namespace: ns.clone(),
                            base: scan_root,
                        },
                    },
                ));
            }
        }
        for (ns, dirs) in &root.autoload.psr0 {
            for dir in dirs {
                let scan_root = canonical(project_root.join(strip_leading_slash(dir)));
                psr_tasks.push((
                    ns.clone(),
                    Task {
                        origin: Origin::Root,
                        scan_root: scan_root.clone(),
                        install_abs: canonical(project_root.to_path_buf()),
                        filter: NamespaceFilter::Psr0 {
                            namespace: ns.clone(),
                            base: scan_root,
                        },
                    },
                ));
            }
        }

        // krsort: reverse-lex by namespace, stable so PSR-4 stays
        // ahead of PSR-0 within the same namespace and per-package
        // entries keep lockfile order within type.
        psr_tasks.sort_by(|a, b| b.0.cmp(&a.0));
        tasks.extend(psr_tasks.into_iter().map(|(_, t)| t));
    }

    // Parallel scan — rayon preserves source order in `collect`, so
    // the sequential merge below sees results in the same order as
    // the lockfile + root iteration.
    let per_task: Vec<Vec<(String, String)>> = tasks
        .par_iter()
        .map(|task| {
            scan::scan(&task.scan_root, &task.filter, &exclude)
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

    // Sequential merge: first-seen wins across tasks. BTreeMap sorts
    // the final output alphabetically for emit.
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

/// Resolve symlinks in a path. Mirrors Composer's `realpath()` usage
/// in `ClassMapGenerator::scanPaths` and `parseAutoloadsType` — every
/// install/scan path is realpath'd before being compared. On macOS
/// the project root often sits under `/var/folders/...` which
/// symlinks to `/private/var/folders/...`; without this normalization
/// the exclude regex (anchored at the canonical form) would never
/// match scan output (using the symlink form), and `strip_prefix` of
/// the install path against file paths would also fail.
///
/// Falls back to the input path when canonicalize fails (target
/// doesn't exist yet, permission denied, etc.) — the surrounding
/// scan returns empty in those cases anyway.
fn canonical(p: PathBuf) -> PathBuf {
    std::fs::canonicalize(&p).unwrap_or(p)
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
