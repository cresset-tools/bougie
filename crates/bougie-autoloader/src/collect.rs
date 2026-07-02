//! Walk the lockfile + root manifest and produce the data shapes the
//! emitters consume.
//!
//! Order matters: Composer's autoload arrays are insertion-ordered,
//! so per-package entries land in lockfile order and root entries
//! come last (matching Composer's own pass order in
//! `AutoloadGenerator`).

use bougie_installers::InstallerPaths;
use md5::{Digest, Md5};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use crate::lock::{LockFile, Package, RootManifest};
use crate::scan::{ExcludePatterns, NamespaceFilter};

/// Project-root-relative install path for a package, honoring
/// `composer/installers` (`type` + root `extra.installer-paths`).
/// Resolves to `vendor/<name>` for the common case, so output is
/// unchanged for any non-relocated package.
fn pkg_install_path(pkg: &Package, installer_paths: &InstallerPaths) -> String {
    bougie_installers::install_path(&pkg.name, pkg.package_type.as_deref(), installer_paths)
}

/// Build the `$vendorDir`/`$baseDir`-anchored PHP path literal for a
/// path `sub` (an already-normalized, slash-separated relative dir or
/// file, possibly empty) under a package's project-relative
/// `install_path`. Packages under `vendor/` anchor on `$vendorDir`
/// (which the emitted autoload files define as `dirname(__DIR__)` =
/// `vendor/`); a package relocated outside `vendor/` anchors on
/// `$baseDir` (the project root) — matching Composer's
/// `findShortestPathCode` choice.
fn path_code(install_path: &str, sub: &str) -> String {
    let full = if sub.is_empty() {
        install_path.to_string()
    } else {
        format!("{install_path}/{sub}")
    };
    match full.strip_prefix("vendor/") {
        Some(rest) => anchored_path_expr("$vendorDir", rest),
        None => anchored_path_expr("$baseDir", &full),
    }
}

/// Build the PHP path expression `<anchor> . '/<path>'`, escaping the
/// path literal exactly as Composer's `var_export` does (backslashes
/// and single quotes). `anchor` is the PHP variable (`$vendorDir` or
/// `$baseDir`); `path` is an already-normalized, slash-separated
/// relative path emitted with a leading `/`. Without this a path
/// containing a `'` or `\` (e.g. a `o'brien/` classmap dir) would emit
/// a syntactically invalid single-quoted literal and break the whole
/// generated autoloader. For paths with no special characters the
/// output is byte-identical to a raw `'/…'` interpolation, so existing
/// fixtures are unaffected.
fn anchored_path_expr(anchor: &str, path: &str) -> String {
    format!("{anchor} . {}", crate::emit::php_single_quoted(&format!("/{path}")))
}

/// One PSR-4 or PSR-0 prefix and its install-path-prefixed dirs.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub prefix: String,
    pub paths: Vec<String>,
}

pub(crate) fn psr4(
    root: &RootManifest,
    lock: &LockFile,
    no_dev: bool,
    installer_paths: &InstallerPaths,
) -> Vec<Entry> {
    aggregate_psr(
        root,
        lock,
        no_dev,
        installer_paths,
        |pkg| &pkg.autoload.psr4,
        |r| &r.autoload.psr4,
        |r| &r.autoload_dev.psr4,
    )
}

pub(crate) fn psr0(
    root: &RootManifest,
    lock: &LockFile,
    no_dev: bool,
    installer_paths: &InstallerPaths,
) -> Vec<Entry> {
    aggregate_psr(
        root,
        lock,
        no_dev,
        installer_paths,
        |pkg| &pkg.autoload.psr0,
        |r| &r.autoload.psr0,
        |r| &r.autoload_dev.psr0,
    )
}

/// Walk root + every package's PSR-* block in Composer's
/// `reverseSortedMap` order (root first, then reverse of the
/// topological sort), aggregating same-prefix entries into a single
/// `Entry` with concatenated paths. Without aggregation, two
/// packages declaring the same namespace would emit duplicate map
/// keys in `autoload_psr4.php` — invalid PHP that silently overrides
/// at runtime — and the path-list order would diverge from Composer's.
fn aggregate_psr<'a, F, G, H>(
    root: &'a RootManifest,
    lock: &'a LockFile,
    no_dev: bool,
    installer_paths: &InstallerPaths,
    psr_pkg: F,
    psr_root: G,
    psr_root_dev: H,
) -> Vec<Entry>
where
    F: Fn(&'a crate::lock::Package) -> &'a Vec<(String, Vec<String>)>,
    G: Fn(&'a RootManifest) -> &'a Vec<(String, Vec<String>)>,
    H: Fn(&'a RootManifest) -> &'a Vec<(String, Vec<String>)>,
{
    let mut out: Vec<Entry> = vec![];
    let push = |out: &mut Vec<Entry>, prefix: &str, path: String| {
        if let Some(existing) = out.iter_mut().find(|e| e.prefix == prefix) {
            existing.paths.push(path);
        } else {
            out.push(Entry {
                prefix: prefix.to_string(),
                paths: vec![path],
            });
        }
    };

    // Root first — matches Composer's parseAutoloadsType iteration
    // (`reverseSortedMap` puts root at the front, then reverse of
    // sortPackages output).
    for (prefix, dirs) in psr_root(root) {
        for d in dirs {
            push(
                &mut out,
                prefix,
                anchored_path_expr("$baseDir", normalize_emit_dir(d)),
            );
        }
    }
    // Root dev autoload — Composer merges the root package's `autoload-dev`
    // into its `autoload` when dev deps are included, so these entries belong
    // with the root block (ahead of any dependency). Skipped under `--no-dev`.
    // Without this the root's `Tests\` (and any dev PSR-* prefix) is dropped
    // from `autoload_psr4.php`, breaking test bootstrapping.
    if !no_dev {
        for (prefix, dirs) in psr_root_dev(root) {
            for d in dirs {
                push(
                    &mut out,
                    prefix,
                    anchored_path_expr("$baseDir", normalize_emit_dir(d)),
                );
            }
        }
    }
    for pkg in lock.reverse_sorted_packages(no_dev) {
        let install_path = pkg_install_path(pkg, installer_paths);
        for (prefix, dirs) in psr_pkg(pkg) {
            for d in dirs {
                push(
                    &mut out,
                    prefix,
                    path_code(&install_path, normalize_emit_dir(d)),
                );
            }
        }
    }
    krsort_entries(&mut out);
    out
}

/// PHP's `krsort` semantics: sort by key (here the namespace prefix)
/// in descending lex order. Composer applies this to the aggregated
/// psr-4/psr-0 maps before emit so the more-specific namespaces hit
/// the runtime `ClassLoader` first. Stable so per-package insertion
/// order is preserved within a prefix bucket.
fn krsort_entries(out: &mut Vec<Entry>) {
    out.sort_by(|a, b| b.prefix.cmp(&a.prefix));
}

#[derive(Debug, Clone)]
pub(crate) struct FileEntry {
    pub identifier: String,
    pub path_expr: String,
}

/// Emit order matches Composer's
/// `parseAutoloads`: `$files = parseAutoloadsType($sortedPackageMap,
/// 'files', ...)` — `$sortedPackageMap` is `sortPackageMap(deps)`
/// with the root appended last. So packages come first in topological
/// (deps-first) order, then root. This matters when one package's
/// `files` autoload references symbols defined in another's at
/// include time; the dependency's file must `require` first.
pub(crate) fn files(
    root: &RootManifest,
    lock: &LockFile,
    no_dev: bool,
    installer_paths: &InstallerPaths,
) -> Vec<FileEntry> {
    let mut out = vec![];
    for pkg in lock.sorted_packages(no_dev) {
        let install_path = pkg_install_path(pkg, installer_paths);
        for f in &pkg.autoload.files {
            out.push(FileEntry {
                identifier: file_identifier(&pkg.name, f),
                path_expr: path_code(&install_path, normalize_emit_dir(f)),
            });
        }
    }
    for f in &root.autoload.files {
        out.push(FileEntry {
            identifier: file_identifier("__root__", f),
            path_expr: anchored_path_expr("$baseDir", strip_leading_slash(f)),
        });
    }
    // Root dev `files` — included with the root block unless `--no-dev`.
    if !no_dev {
        for f in &root.autoload_dev.files {
            out.push(FileEntry {
                identifier: file_identifier("__root__", f),
                path_expr: anchored_path_expr("$baseDir", strip_leading_slash(f)),
            });
        }
    }
    out
}

/// One classmap row: `'<class>' => <path expression>`.
#[derive(Debug, Clone)]
pub(crate) struct ClassmapEntry {
    pub class: String,
    pub path_expr: String,
}

/// Which side of the `$vendorDir`/`$baseDir` split owns the path
/// expression a task emits. Owned so a `Task` can outlive the
/// `LockFile` borrow that produced it (the live `Autoloader` keeps
/// the task list across many incremental edits).
#[derive(Debug, Clone)]
pub(crate) enum Origin {
    /// A package's scan. Carries the package's project-relative install
    /// path (e.g. `vendor/acme/foo`, or a relocated `app/design/...`) so
    /// the emitted classmap literal anchors on the right base.
    Package { install_path: String },
    Root,
}

/// One scan task — the unit the classmap-build pipeline parallelizes
/// over. Same shape as Composer's per-`scanPaths` invocation in
/// `AutoloadGenerator::dump`: a scan root, the install path the
/// emitted PHP literal is anchored against, the per-class namespace
/// filter (for `-o`-mode PSR-* scans; `None` for plain classmap
/// dirs), and whether vendor needs to be auto-excluded.
#[derive(Debug, Clone)]
pub(crate) struct Task {
    pub origin: Origin,
    pub install_abs: PathBuf,
    pub scan_root: PathBuf,
    pub filter: NamespaceFilter,
    /// True only for `-o`-mode PSR-* tasks whose `scan_root` spans
    /// the project's vendor/ tree. Mirrors Composer's `dump()`:
    /// `if (str_contains($vendorPath, $dir.'/'))` adds vendor to
    /// the exclude regex for that specific scan, otherwise the
    /// scan would walk through vendor/ and possibly classmap a
    /// vendor file under the user's namespace.
    pub needs_vendor_exclude: bool,
}

/// Outputs of [`build_classmap_tasks`]: the ordered task list plus
/// the two precompiled exclude sets the tasks pick between based on
/// their `needs_vendor_exclude` flag.
pub(crate) struct TaskSet {
    pub tasks: Vec<Task>,
    pub exclude_default: ExcludePatterns,
    pub exclude_with_vendor: ExcludePatterns,
}

/// Build the classmap scan task list — Composer's `dump()` order:
/// root classmap dirs, then package classmap dirs in reverseSortedMap
/// order, then (when `optimize`) the PSR-* dirs across root + packages
/// `krsort`'d by namespace.
///
/// Returns the task list plus two precompiled exclude sets: the
/// default and a "default + project's vendor/" variant the optimize-
/// mode PSR-* scans use when their `scan_root` spans vendor.
pub(crate) fn build_classmap_tasks(
    root: &RootManifest,
    lock: &LockFile,
    no_dev: bool,
    optimize: bool,
    project_root: &Path,
    installer_paths: &InstallerPaths,
) -> TaskSet {
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
        let install_abs = canonical(project_root.join(pkg_install_path(pkg, installer_paths)));
        for raw in &pkg.autoload.exclude_from_classmap {
            exclude_patterns.push((install_abs.clone(), raw.clone()));
        }
    }
    for raw in &root.autoload.exclude_from_classmap {
        exclude_patterns.push((canonical(project_root.to_path_buf()), raw.clone()));
    }
    if !no_dev {
        for raw in &root.autoload_dev.exclude_from_classmap {
            exclude_patterns.push((canonical(project_root.to_path_buf()), raw.clone()));
        }
    }
    let exclude_default = ExcludePatterns::build(&exclude_patterns);

    // Second pre-compiled set with the project's `vendor/` appended.
    // Used by PSR-* scan tasks whose scan_root contains vendor (i.e.
    // a root mapping like `"App\\": "."`). The scan would otherwise
    // walk into vendor/ and the filter would let through any vendor
    // file whose path happens to match the user's namespace+path
    // rule. We mirror Composer's per-scan exclude.
    let project_root_abs = canonical(project_root.to_path_buf());
    let vendor_abs = canonical(project_root.join("vendor"));
    let mut exclude_with_vendor_patterns = exclude_patterns.clone();
    exclude_with_vendor_patterns.push((project_root_abs.clone(), "vendor/".to_string()));
    let exclude_with_vendor = ExcludePatterns::build(&exclude_with_vendor_patterns);

    let mut tasks: Vec<Task> = Vec::new();

    // Classmap dirs — matches Composer's dump() order:
    // parseAutoloadsType iterates reverseSortedMap (root first, then
    // reverse of topological dependency sort), and the scan iterates
    // the resulting aggregated `classmap` list. Root entries appear
    // first in that list, so we scan them first; subsequent packages
    // come in reverse-sorted order.
    for dir in &root.autoload.classmap {
        tasks.push(Task {
            origin: Origin::Root,
            scan_root: canonical(project_root.join(strip_leading_slash(dir))),
            install_abs: project_root_abs.clone(),
            filter: NamespaceFilter::None,
            // Composer does NOT vendor-guard classmap dirs — those are
            // explicitly listed and assumed scoped already. Only the
            // PSR-* scan gets the auto-exclude.
            needs_vendor_exclude: false,
        });
    }
    // Root dev classmap dirs — merged into the root block by Composer when
    // dev deps are included; sit with the root entries, before any package.
    if !no_dev {
        for dir in &root.autoload_dev.classmap {
            tasks.push(Task {
                origin: Origin::Root,
                scan_root: canonical(project_root.join(strip_leading_slash(dir))),
                install_abs: project_root_abs.clone(),
                filter: NamespaceFilter::None,
                needs_vendor_exclude: false,
            });
        }
    }
    for pkg in lock.reverse_sorted_packages(no_dev) {
        if pkg.autoload.classmap.is_empty() {
            continue;
        }
        let install_path = pkg_install_path(pkg, installer_paths);
        let install_abs = canonical(project_root.join(&install_path));
        for dir in &pkg.autoload.classmap {
            tasks.push(Task {
                origin: Origin::Package { install_path: install_path.clone() },
                scan_root: canonical(install_abs.join(strip_leading_slash(dir))),
                install_abs: install_abs.clone(),
                filter: NamespaceFilter::None,
                needs_vendor_exclude: false,
            });
        }
    }

    if optimize {
        // Composer's `dump()` buckets all PSR-* entries by namespace
        // (across packages and root, in reverseSortedMap order), then
        // runs `krsort` on the bucket keys so more-specific namespaces
        // scan first. Within a bucket the order is reverseSortedMap
        // order (root first, then reverse of topological dep sort).
        //
        // We collect candidate tasks tagged with their namespace,
        // emit them in reverseSortedMap order (root first, then
        // reversed sortPackages), then stable-sort by namespace
        // descending so PSR-4 stays before PSR-0 within a namespace
        // bucket (mirrors Composer's `foreach (['psr-4', 'psr-0']
        // ...)` outer-loop order).
        // Composer's per-scan vendor-dir guard:
        //   `if (str_contains($vendorPath, $dir.'/'))` ⇒ add vendor to
        // the exclude regex. We mirror it with a path-prefix check
        // (`vendor_abs` starts with `scan_root` and isn't equal).
        let spans_vendor = |scan_root: &Path| -> bool {
            vendor_abs != *scan_root && vendor_abs.starts_with(scan_root)
        };

        let mut psr_tasks: Vec<(String, Task)> = Vec::new();
        for (ns, dirs) in &root.autoload.psr4 {
            for dir in dirs {
                let scan_root = canonical(project_root.join(strip_leading_slash(dir)));
                let needs_vendor_exclude = spans_vendor(&scan_root);
                psr_tasks.push((
                    ns.clone(),
                    Task {
                        origin: Origin::Root,
                        scan_root: scan_root.clone(),
                        install_abs: project_root_abs.clone(),
                        filter: NamespaceFilter::Psr4 {
                            namespace: ns.clone(),
                            base: scan_root,
                        },
                        needs_vendor_exclude,
                    },
                ));
            }
        }
        // Root dev PSR-4 — Composer merges `autoload-dev` into the root
        // package, so its prefixes (e.g. `Tests\`) are scanned into the
        // optimized classmap too (root-first, ahead of dependencies).
        if !no_dev {
            for (ns, dirs) in &root.autoload_dev.psr4 {
                for dir in dirs {
                    let scan_root = canonical(project_root.join(strip_leading_slash(dir)));
                    let needs_vendor_exclude = spans_vendor(&scan_root);
                    psr_tasks.push((
                        ns.clone(),
                        Task {
                            origin: Origin::Root,
                            scan_root: scan_root.clone(),
                            install_abs: project_root_abs.clone(),
                            filter: NamespaceFilter::Psr4 {
                                namespace: ns.clone(),
                                base: scan_root,
                            },
                            needs_vendor_exclude,
                        },
                    ));
                }
            }
        }
        for (ns, dirs) in &root.autoload.psr0 {
            for dir in dirs {
                let scan_root = canonical(project_root.join(strip_leading_slash(dir)));
                let needs_vendor_exclude = spans_vendor(&scan_root);
                psr_tasks.push((
                    ns.clone(),
                    Task {
                        origin: Origin::Root,
                        scan_root: scan_root.clone(),
                        install_abs: project_root_abs.clone(),
                        filter: NamespaceFilter::Psr0 {
                            namespace: ns.clone(),
                            base: scan_root,
                        },
                        needs_vendor_exclude,
                    },
                ));
            }
        }
        // Root dev PSR-0 — same merge rule as dev PSR-4 above.
        if !no_dev {
            for (ns, dirs) in &root.autoload_dev.psr0 {
                for dir in dirs {
                    let scan_root = canonical(project_root.join(strip_leading_slash(dir)));
                    let needs_vendor_exclude = spans_vendor(&scan_root);
                    psr_tasks.push((
                        ns.clone(),
                        Task {
                            origin: Origin::Root,
                            scan_root: scan_root.clone(),
                            install_abs: project_root_abs.clone(),
                            filter: NamespaceFilter::Psr0 {
                                namespace: ns.clone(),
                                base: scan_root,
                            },
                            needs_vendor_exclude,
                        },
                    ));
                }
            }
        }
        for pkg in lock.reverse_sorted_packages(no_dev) {
            let install_path = pkg_install_path(pkg, installer_paths);
            let install_abs = canonical(project_root.join(&install_path));
            for (ns, dirs) in &pkg.autoload.psr4 {
                for dir in dirs {
                    let scan_root = canonical(install_abs.join(strip_leading_slash(dir)));
                    let needs_vendor_exclude = spans_vendor(&scan_root);
                    psr_tasks.push((
                        ns.clone(),
                        Task {
                            origin: Origin::Package { install_path: install_path.clone() },
                            scan_root: scan_root.clone(),
                            install_abs: install_abs.clone(),
                            filter: NamespaceFilter::Psr4 {
                                namespace: ns.clone(),
                                base: scan_root,
                            },
                            needs_vendor_exclude,
                        },
                    ));
                }
            }
            for (ns, dirs) in &pkg.autoload.psr0 {
                for dir in dirs {
                    let scan_root = canonical(install_abs.join(strip_leading_slash(dir)));
                    let needs_vendor_exclude = spans_vendor(&scan_root);
                    psr_tasks.push((
                        ns.clone(),
                        Task {
                            origin: Origin::Package { install_path: install_path.clone() },
                            scan_root: scan_root.clone(),
                            install_abs: install_abs.clone(),
                            filter: NamespaceFilter::Psr0 {
                                namespace: ns.clone(),
                                base: scan_root,
                            },
                            needs_vendor_exclude,
                        },
                    ));
                }
            }
        }

        // krsort: reverse-lex by namespace, stable so PSR-4 stays
        // ahead of PSR-0 within the same namespace and per-package
        // entries keep reverseSortedMap order within type.
        psr_tasks.sort_by(|a, b| b.0.cmp(&a.0));
        tasks.extend(psr_tasks.into_iter().map(|(_, t)| t));
    }

    TaskSet {
        tasks,
        exclude_default,
        exclude_with_vendor,
    }
}

/// Build the path-expression literal a classmap row emits for a file
/// at `rel` (relative to `task.install_abs`). Used by both the
/// bootstrap merge and the live-patch flow so the two produce
/// byte-identical output.
pub(crate) fn task_path_expr(task: &Task, rel: &Path) -> String {
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    match &task.origin {
        Origin::Package { install_path } => path_code(install_path, &rel_str),
        Origin::Root => anchored_path_expr("$baseDir", &rel_str),
    }
}

/// Always-present synthetic classmap row Composer emits even when no
/// user classes are found. Pulled out so bootstrap, live-patch
/// re-merge, and tests reference one definition.
pub(crate) fn installed_versions_row() -> (String, String) {
    (
        "Composer\\InstalledVersions".to_string(),
        "$vendorDir . '/composer/InstalledVersions.php'".to_string(),
    )
}

/// Composer's `AutoloadGenerator::getFileIdentifier`:
/// `md5(package_name + ':' + path)`. Uses `RustCrypto`'s `md-5` — the
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

pub(crate) fn strip_leading_slash(s: &str) -> &str {
    s.strip_prefix('/').unwrap_or(s)
}

/// Normalize a PSR-* directory path the way Composer's
/// `Filesystem::normalizePath` + `findShortestPath` does before emit:
/// strip a leading `/`, strip a leading `./`, treat a lone `.` as
/// empty, trim trailing `/`. Returns a borrowed slice — the caller
/// formats the result into the emitted PHP literal.
fn normalize_emit_dir(d: &str) -> &str {
    let d = d.strip_prefix('/').unwrap_or(d);
    if d == "." {
        return "";
    }
    let d = d.strip_prefix("./").unwrap_or(d);
    d.trim_end_matches('/')
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
pub(crate) fn canonical(p: PathBuf) -> PathBuf {
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

    #[test]
    fn path_code_leaves_ordinary_paths_byte_identical() {
        // The common case must be unchanged so golden fixtures still match.
        assert_eq!(
            path_code("vendor/monolog/monolog", "src"),
            "$vendorDir . '/monolog/monolog/src'"
        );
        assert_eq!(
            path_code("wp-content/plugins/akismet", ""),
            "$baseDir . '/wp-content/plugins/akismet'"
        );
    }

    #[test]
    fn path_code_escapes_quotes_and_backslashes() {
        // A dir with a single quote must be escaped like Composer's
        // `var_export`, or the emitted single-quoted literal is invalid
        // PHP and breaks the entire autoloader.
        assert_eq!(
            path_code("vendor/acme/pkg", "o'brien"),
            r"$vendorDir . '/acme/pkg/o\'brien'"
        );
        assert_eq!(
            path_code("acme/pkg", r"we\ird"),
            r"$baseDir . '/acme/pkg/we\\ird'"
        );
    }
}
