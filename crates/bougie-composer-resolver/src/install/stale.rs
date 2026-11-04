//! Removing packages that left the lock.
//!
//! An install has to know what the *previous* install put on disk, so it
//! can delete what the new lock no longer has. Two records answer that:
//!
//! - `vendor/composer/installed.json`, Composer's own record. bougie
//!   rewrites it on every autoloader dump, and the autoloader is built
//!   from `composer.lock`, not from `vendor/`. So a dump between a lock
//!   change and the next install — `bougie server` re-bootstrapping on
//!   the lock change, or a manual `bougie composer dump-autoload` —
//!   drops the old package from it while its directory is still there.
//!   The next install then never learns to remove it.
//! - [`InstallRecord`] at `vendor/bougie/state/installed-packages.json`,
//!   written only at the end of a successful install. Nothing else
//!   touches it, so it survives those dumps.
//!
//! The sweep removes the union of both, each at the path it was actually
//! installed to (`installer-paths` can move a package out of
//! `vendor/<name>`), and never a path a wanted package lives at.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

use bougie_composer::lockfile::LockPackage;
use bougie_installers::InstallerPaths;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped if the on-disk shape changes; an unknown version reads as
/// "no record" rather than being misinterpreted.
const RECORD_VERSION: u32 = 1;

/// bougie's own record of what the last successful install put on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InstallRecord {
    version: u32,
    /// Package name → install path relative to the project root, with
    /// `/` separators (`vendor/acme/foo`, `app/code/Acme/Foo`).
    packages: BTreeMap<String, String>,
}

impl InstallRecord {
    /// The record for an install of `packages`: every package that gets
    /// a directory (metapackages don't), at its installer path.
    pub(crate) fn for_packages(
        packages: &[&LockPackage],
        installer_paths: &InstallerPaths,
    ) -> Self {
        let packages = packages
            .iter()
            .filter(|p| !p.is_metapackage())
            .map(|p| {
                let rel = bougie_installers::install_path(
                    &p.name,
                    p.package_type.as_deref(),
                    installer_paths,
                );
                (p.name.clone(), rel)
            })
            .collect();
        Self {
            version: RECORD_VERSION,
            packages,
        }
    }

    /// Read the record, or `None` when it is missing, unreadable, or
    /// written by an incompatible version.
    pub(crate) fn read(project_root: &Path) -> Option<Self> {
        let bytes = std::fs::read(record_path(project_root)).ok()?;
        let record: Self = serde_json::from_slice(&bytes).ok()?;
        (record.version == RECORD_VERSION).then_some(record)
    }

    /// Write the record atomically (temp file + rename), so an
    /// interrupted write leaves the previous record intact.
    pub(crate) fn write(&self, project_root: &Path) -> std::io::Result<()> {
        let path = record_path(project_root);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)
    }
}

fn record_path(project_root: &Path) -> PathBuf {
    bougie_paths::project::installed_packages(project_root)
}

/// Package name → project-relative install path, from
/// `vendor/composer/installed.json`'s `install-path` (stored relative to
/// `vendor/composer`). Entries without one, or whose path can't be
/// trusted, map to `None` and fall back to `vendor/<name>`.
fn installed_json_paths(project_root: &Path) -> HashMap<String, Option<String>> {
    let Some(packages) = std::fs::read(project_root.join("vendor/composer/installed.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("packages")?.as_array().cloned())
    else {
        return HashMap::new();
    };
    packages
        .iter()
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?;
            if name.is_empty() {
                return None;
            }
            let path = p
                .get("install-path")
                .and_then(Value::as_str)
                .and_then(|ip| normalize_relative(&format!("vendor/composer/{ip}")));
            Some((name.to_string(), path))
        })
        .collect()
}

/// Lexically normalize a `/`-separated relative path. `None` when it is
/// absolute or climbs above its base — such a path is never ours to
/// delete.
fn normalize_relative(path: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for c in Path::new(path).components() {
        match c {
            Component::Normal(s) => out.push(s.to_str()?),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out.join("/"))
}

/// Paths that hold more than one package's files, or bougie's own state.
/// Never removed, whatever a record claims.
const PROTECTED: &[&str] = &[
    "",
    "vendor",
    "vendor/composer",
    "vendor/bin",
    "vendor/bougie",
];

/// `a` equals `b` or is one of its ancestors (both normalized).
fn is_same_or_ancestor(a: &str, b: &str) -> bool {
    a == b || a.is_empty() || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
}

/// Remove every previously installed package that `wanted` no longer
/// has. Returns how many directories were removed.
///
/// `wanted` is the new install's [`InstallRecord`]: those names are kept,
/// and so is anything at, above, or below one of their paths (two
/// packages can share an installer path, and the new one must survive).
pub(crate) fn remove_stale_packages(project_root: &Path, wanted: &InstallRecord) -> u32 {
    let previous = previous_install(project_root);
    let wanted_paths: Vec<&str> = wanted.packages.values().map(String::as_str).collect();
    let mut removed: u32 = 0;
    for (name, rel) in &previous {
        if wanted.packages.contains_key(name) || PROTECTED.contains(&rel.as_str()) {
            continue;
        }
        if wanted_paths
            .iter()
            .any(|w| is_same_or_ancestor(rel, w) || is_same_or_ancestor(w, rel))
        {
            continue;
        }
        if remove_package_dir(project_root, rel) {
            removed = removed.saturating_add(1);
        }
    }
    removed
}

/// Name → normalized install path for everything the previous install
/// may have left: [`InstallRecord`] first (it survives autoloader
/// dumps), then `installed.json`, then the `vendor/<name>` default.
fn previous_install(project_root: &Path) -> BTreeMap<String, String> {
    let mut previous: BTreeMap<String, String> = BTreeMap::new();
    if let Some(record) = InstallRecord::read(project_root) {
        for (name, rel) in record.packages {
            if let Some(rel) = normalize_relative(&rel) {
                previous.insert(name, rel);
            }
        }
    }
    for (name, rel) in installed_json_paths(project_root) {
        let rel = rel.or_else(|| normalize_relative(&format!("vendor/{name}")));
        if let Some(rel) = rel {
            previous.entry(name).or_insert(rel);
        }
    }
    previous
}

/// Remove one package directory (or the symlink a `symlink: true` path
/// repository left). Prunes the now-empty `vendor/<vendor>` parent, as
/// Composer does. `false` when there was nothing to remove.
fn remove_package_dir(project_root: &Path, rel: &str) -> bool {
    let dir = project_root.join(rel);
    let Ok(meta) = std::fs::symlink_metadata(&dir) else {
        return false;
    };
    let ok = if meta.file_type().is_symlink() {
        // Remove the link, never what it points at. On Windows a
        // directory symlink or junction needs `remove_dir`.
        std::fs::remove_file(&dir)
            .or_else(|_| std::fs::remove_dir(&dir))
            .is_ok()
    } else if meta.is_dir() {
        std::fs::remove_dir_all(&dir).is_ok()
    } else {
        false
    };
    if ok
        && let Some(parent) = dir.parent()
        && parent != project_root.join("vendor")
        && parent.starts_with(project_root.join("vendor"))
    {
        // Only succeeds when empty.
        let _ = std::fs::remove_dir(parent);
    }
    ok
}

#[cfg(test)]
mod tests;
