//! Install-time materialization of `type: path` packages.
//!
//! A path package isn't downloaded — its source already lives on disk
//! at the project-relative `dist.url`. Composer materializes it into
//! `vendor/<name>` by **symlinking** the source (falling back to a
//! recursive **copy** when symlinking fails or `transport-options`
//! requests it). This module is that step; the zip downloader skips
//! path dists entirely.
//!
//! Behavior mirrors Composer's `PathDownloader`:
//! - default: symlink; on failure, copy (with a warning).
//! - `transport-options.symlink == false`: always copy.
//! - `transport-options.relative == true`: the symlink target is a
//!   relative path (portable across a moved/mounted tree).
//! - re-install is idempotent: an unchanged reference with the dest
//!   already in place is left alone; otherwise the dest is replaced.

use std::path::{Path, PathBuf};

use bougie_composer::lockfile::LockPackage;
use serde_json::Value;

use super::orchestrate::InstalledState;

/// Result of the path-materialization pass.
#[derive(Debug, Default)]
pub(crate) struct PathLinkSummary {
    /// Path packages newly linked or re-materialized this run.
    pub(crate) linked: u32,
    /// Path packages already in place with an unchanged reference.
    pub(crate) up_to_date: u32,
    pub(crate) warnings: Vec<String>,
}

/// Materialize every path package into its `vendor/` destination.
///
/// `dests[i]` is the already-computed install directory for
/// `packages[i]` (the same `install_path` mapping the zip packages and
/// the autoloader use, so a `composer/installers` relocation is
/// honored). `installed_state` drives idempotency: a package whose
/// recorded dist reference matches the lock and whose dest is present
/// is left untouched.
pub(crate) fn materialize_path_packages(
    project_root: &Path,
    packages: &[&LockPackage],
    dests: &[PathBuf],
    installed_state: Option<&InstalledState>,
) -> PathLinkSummary {
    let mut summary = PathLinkSummary::default();
    for (pkg, dest) in packages.iter().zip(dests.iter()) {
        let Some(dist) = pkg.dist.as_ref() else { continue };
        let source = project_root.join(&dist.url);
        if !source.exists() {
            summary.warnings.push(format!(
                "{}: path source {} does not exist; skipping",
                pkg.name,
                source.display(),
            ));
            continue;
        }

        // Idempotency: unchanged reference + dest already present ⇒
        // nothing to do. `reference: none` packages have an empty
        // reference, so they re-link only when the dest is missing.
        let lock_ref = dist.reference.as_deref().unwrap_or("");
        let installed_ref = installed_state
            .and_then(|s| s.packages.get(&pkg.name))
            .map(String::as_str);
        if dest_in_place(dest, &source) && installed_ref == Some(lock_ref) && !lock_ref.is_empty() {
            summary.up_to_date = summary.up_to_date.saturating_add(1);
            continue;
        }

        match materialize_one(&source, dest, pkg) {
            Ok(()) => summary.linked = summary.linked.saturating_add(1),
            Err(e) => summary.warnings.push(format!(
                "{}: failed to install path package from {}: {e}",
                pkg.name,
                source.display(),
            )),
        }
    }
    summary
}

/// Whether `dest` is already a usable materialization of `source` — a
/// symlink resolving to `source`, or a directory that exists (copy
/// mode). Conservative: returns false on any uncertainty so the caller
/// re-materializes.
fn dest_in_place(dest: &Path, source: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(dest) else {
        return false;
    };
    if meta.file_type().is_symlink() {
        match (std::fs::canonicalize(dest), std::fs::canonicalize(source)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    } else {
        dest.is_dir()
    }
}

/// Symlink-or-copy one path package's source into `dest`, honoring its
/// `transport-options` and Composer's `COMPOSER_MIRROR_PATH_REPOS`
/// escape hatch.
fn materialize_one(source: &Path, dest: &Path, pkg: &LockPackage) -> eyre::Result<()> {
    let (symlink_opt, relative) = transport_options(&pkg.transport_options);

    // Clear any prior materialization so the result is exactly the new
    // source (stale symlink, leftover copy, or wrong target).
    remove_existing(dest)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // `symlink: false` forces a copy; so does the
    // `COMPOSER_MIRROR_PATH_REPOS` env (Composer's global mirror
    // switch). Otherwise prefer a link and fall back to a copy.
    let force_mirror = std::env::var_os("COMPOSER_MIRROR_PATH_REPOS").is_some();
    let want_copy = symlink_opt == Some(false) || force_mirror;
    if !want_copy {
        match link_dir(source, dest, relative) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::debug!(
                    package = %pkg.name, error = %e,
                    "linking path package failed; falling back to copy",
                );
            }
        }
    }
    copy_tree(source, dest)
}

/// Read `{symlink, relative}` from a package's `transport-options`.
/// Defaults match Composer's `PathDownloader`: `symlink` unset → prefer
/// a link (`None`); `relative` unset → **true** (a relative link).
fn transport_options(value: &Value) -> (Option<bool>, bool) {
    let Some(obj) = value.as_object() else {
        return (None, true);
    };
    let symlink = obj.get("symlink").and_then(Value::as_bool);
    let relative = obj.get("relative").and_then(Value::as_bool).unwrap_or(true);
    (symlink, relative)
}

/// Remove whatever is currently at `dest` (file, symlink, or
/// directory). A dangling symlink reports as a file to `remove_file`.
fn remove_existing(dest: &Path) -> eyre::Result<()> {
    match std::fs::symlink_metadata(dest) {
        Ok(meta) => {
            if meta.file_type().is_dir() {
                std::fs::remove_dir_all(dest)?;
            } else {
                std::fs::remove_file(dest)?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Compute a relative symlink target: `source` expressed relative to
/// `dest`'s parent directory. Falls back to the absolute source if no
/// relative form is computable. Unix-only — Windows junctions are
/// absolute.
#[cfg(unix)]
fn relative_symlink_target(dest: &Path, source: &Path) -> PathBuf {
    use std::path::Component;
    let base = dest.parent().unwrap_or(dest);
    let base_comps: Vec<Component> = base.components().collect();
    let src_comps: Vec<Component> = source.components().collect();
    let common = base_comps
        .iter()
        .zip(&src_comps)
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        return source.to_path_buf();
    }
    let mut rel = PathBuf::new();
    for _ in 0..(base_comps.len() - common) {
        rel.push("..");
    }
    for comp in &src_comps[common..] {
        rel.push(comp.as_os_str());
    }
    if rel.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        rel
    }
}

/// Link `source` into `link` as a directory, the way Composer does:
///
/// - **Unix:** a symbolic link. When `relative` is set the target is
///   expressed relative to the link's parent (Composer's default).
/// - **Windows:** an NTFS **junction**, not a symlink — junctions need
///   no elevated privilege (a Windows symlink needs `SeCreateSymbolicLink`
///   / Developer Mode), so this is what Composer uses. Junctions are
///   absolute-only, so `relative` is ignored on Windows.
///
/// Any failure returns `Err` and the caller falls back to a copy.
#[cfg(unix)]
fn link_dir(source: &Path, link: &Path, relative: bool) -> std::io::Result<()> {
    let target = if relative {
        relative_symlink_target(link, source)
    } else {
        source.to_path_buf()
    };
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn link_dir(source: &Path, link: &Path, _relative: bool) -> std::io::Result<()> {
    // Junctions require an absolute target on a local volume. Resolve
    // the source to an absolute path; junction creation itself needs no
    // privilege (unlike a Windows symbolic link).
    let target = std::fs::canonicalize(source)?;
    junction::create(target, link)
}

#[cfg(not(any(unix, windows)))]
fn link_dir(_source: &Path, _link: &Path, _relative: bool) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "directory links unsupported on this platform",
    ))
}

/// Recursively copy a directory tree from `source` to `dest`. Plain
/// file/dir copy — symlinks inside the source are followed (Composer's
/// mirror copy does the same). Used as the symlink fallback and for
/// `symlink: false`.
fn copy_tree(source: &Path, dest: &Path) -> eyre::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// Build a project tree with a path package source at `pkg_src`
    /// (relative to root), and return (root, the LockPackage). The
    /// package's `dist.url` is `pkg_src` and it carries the given
    /// transport-options.
    fn setup(transport: serde_json::Value, reference: Option<&str>) -> (TempDir, LockPackage) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let src = root.join("packages/local");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("composer.json"), b"{\"name\":\"acme/local\"}").unwrap();
        std::fs::write(src.join("marker.txt"), b"hello").unwrap();

        let mut dist = serde_json::json!({
            "type": "path",
            "url": "packages/local",
        });
        if let Some(r) = reference {
            dist["reference"] = serde_json::json!(r);
        }
        let pkg: LockPackage = serde_json::from_value(serde_json::json!({
            "name": "acme/local",
            "version": "1.0.0",
            "dist": dist,
            "transport-options": transport,
        }))
        .unwrap();
        (tmp, pkg)
    }

    #[cfg(unix)]
    #[test]
    fn default_mode_symlinks() {
        let (tmp, pkg) = setup(Value::Null, None);
        let root = tmp.path();
        let dest = root.join("vendor/acme/local");
        let summary =
            materialize_path_packages(root, &[&pkg], std::slice::from_ref(&dest), None);
        assert_eq!(summary.linked, 1);
        assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);
        let meta = std::fs::symlink_metadata(&dest).unwrap();
        assert!(meta.file_type().is_symlink(), "default mode must symlink");
        // The symlink resolves to the source and exposes its files.
        assert_eq!(
            std::fs::read_to_string(dest.join("marker.txt")).unwrap(),
            "hello",
        );
    }

    #[test]
    fn symlink_false_copies() {
        let (tmp, pkg) = setup(serde_json::json!({"symlink": false}), None);
        let root = tmp.path();
        let dest = root.join("vendor/acme/local");
        let summary =
            materialize_path_packages(root, &[&pkg], std::slice::from_ref(&dest), None);
        assert_eq!(summary.linked, 1);
        let meta = std::fs::symlink_metadata(&dest).unwrap();
        assert!(meta.file_type().is_dir(), "symlink:false must copy a real dir");
        assert!(!meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(dest.join("marker.txt")).unwrap(),
            "hello",
        );
    }

    #[cfg(unix)]
    #[test]
    fn relative_symlink_target_is_relative() {
        let (tmp, pkg) = setup(serde_json::json!({"relative": true}), None);
        let root = tmp.path();
        let dest = root.join("vendor/acme/local");
        materialize_path_packages(root, &[&pkg], std::slice::from_ref(&dest), None);
        let target = std::fs::read_link(&dest).unwrap();
        assert!(target.is_relative(), "target was {target:?}");
        // vendor/acme/local → ../../packages/local
        assert_eq!(target, Path::new("../../packages/local"));
    }

    #[test]
    fn unchanged_reference_is_up_to_date() {
        let (tmp, pkg) = setup(serde_json::json!({"symlink": false}), Some("abc123"));
        let root = tmp.path();
        let dest = root.join("vendor/acme/local");
        // First install.
        let s1 = materialize_path_packages(root, &[&pkg], std::slice::from_ref(&dest), None);
        assert_eq!(s1.linked, 1);
        // Second install with the reference recorded as already-installed.
        let mut packages = HashMap::new();
        packages.insert("acme/local".to_string(), "abc123".to_string());
        let state = Some(InstalledState { packages });
        let s2 = materialize_path_packages(root, &[&pkg], std::slice::from_ref(&dest), state.as_ref());
        assert_eq!(s2.up_to_date, 1, "unchanged reference → up-to-date");
        assert_eq!(s2.linked, 0);
    }

    #[test]
    fn missing_source_warns() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let pkg: LockPackage = serde_json::from_value(serde_json::json!({
            "name": "acme/gone",
            "version": "1.0.0",
            "dist": {"type": "path", "url": "packages/gone"},
        }))
        .unwrap();
        let dest = root.join("vendor/acme/gone");
        let summary =
            materialize_path_packages(root, &[&pkg], std::slice::from_ref(&dest), None);
        assert_eq!(summary.linked, 0);
        assert_eq!(summary.warnings.len(), 1);
        assert!(summary.warnings[0].contains("does not exist"));
    }
}
