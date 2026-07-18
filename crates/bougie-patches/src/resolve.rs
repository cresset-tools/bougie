//! Build the resolved patch set from the **root** project.
//!
//! Per bougie's root-only policy, patch rules come only from the root
//! `composer.json` (never from dependency `extra` — see the crate docs). Two
//! root sources, in cweagans' mutually-exclusive precedence:
//!
//! 1. `extra.patches` (inline, compact or expanded), else
//! 2. an external patches file: v1 top-level `extra.patches-file` or v2
//!    `extra.composer-patches.patches-file`. The file is a JSON object whose
//!    top-level `patches` key holds the same `target → entries` map.
//!
//! The `patches/` directory (Phase C) is unioned on top by the caller.

use std::path::{Path, PathBuf};

use eyre::{Context, Result, bail};
use serde_json::Value;

use crate::model::{Patch, PatchSource, parse_target_patches};
use crate::{diff, target};

/// Resolve the inline + external-file patch rules declared by the root
/// `composer.json` value. Returns the flattened list of [`Patch`]es (one per
/// rule, across all target packages), in declaration order.
///
/// `project_root` is used only to read a relative `patches-file`.
pub fn resolve_root(composer_json: &Value, project_root: &Path) -> Result<Vec<Patch>> {
    let extra = composer_json.get("extra");

    // 1. Inline `extra.patches` wins outright (cweagans v1 early-return).
    if let Some(inline) = extra.and_then(|e| e.get("patches")) {
        return flatten_patch_map(inline);
    }

    // 2. Otherwise an external patches file, either dialect's key.
    if let Some(file) = patches_file_path(extra) {
        let abs = project_root.join(file);
        let bytes = std::fs::read(&abs)
            .wrap_err_with(|| format!("reading patches-file `{}`", abs.display()))?;
        let doc: Value = serde_json::from_slice(&bytes)
            .wrap_err_with(|| format!("parsing patches-file `{}`", abs.display()))?;
        let map = doc.get("patches").ok_or_else(|| {
            eyre::eyre!(
                "patches-file `{}` has no top-level `patches` key",
                abs.display()
            )
        })?;
        return flatten_patch_map(map);
    }

    Ok(Vec::new())
}

/// Resolve the zero-config `patches/` directory: every `*.patch` file (other
/// extensions are ignored so notes/originals can sit alongside), with the
/// target package + depth **inferred from the diff headers**
/// ([`target::infer_target`]).
///
/// `install_paths` maps each locked package to its install directory (the host
/// computes these from the lock).
///
/// `exclude` lists patch files already covered by an explicit declaration
/// (`extra.patches` / patches-file): the scan skips those — the declaration
/// is authoritative for target and depth, and inferring them here anyway
/// would apply the same patch twice (or hard-error on a package-relative
/// file the declaration handles fine). Paths are compared canonically, so a
/// declaration's relative path matches the scan's absolute one.
///
/// A remaining `.patch` whose headers can't be resolved to exactly one
/// installed package is a hard error naming the file — bougie does not
/// silently misapply.
pub fn resolve_patches_dir(
    dir: &Path,
    install_paths: &[(String, String)],
    exclude: &[PathBuf],
) -> Result<Vec<Patch>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let excluded: Vec<PathBuf> = exclude
        .iter()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();

    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .wrap_err_with(|| format!("reading patches dir `{}`", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "patch"))
        .filter(|p| {
            let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            !excluded.contains(&canon)
        })
        .collect();
    files.sort();

    let mut out = Vec::with_capacity(files.len());
    for path in files {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("reading patch `{}`", path.display()))?;
        let parsed = diff::split(&text).wrap_err_with(|| format!("parsing `{name}`"))?;
        let header_paths: Vec<&str> = parsed.iter().filter_map(diff::FileDiff::routed_path).collect();
        let inferred = target::infer_target(&header_paths, install_paths)
            .wrap_err_with(|| format!("patches/{name}"))?;
        out.push(Patch {
            target: inferred.target,
            description: name,
            source: PatchSource::Local(path),
            sha256: None,
            depth: inferred.depth,
            extra: None,
            scope: inferred.scope,
        });
    }
    Ok(out)
}

/// The configured external patches-file path, preferring the v2 namespaced
/// key over the v1 top-level one (v2 is the more explicit form).
fn patches_file_path(extra: Option<&Value>) -> Option<String> {
    let extra = extra?;
    extra
        .get("composer-patches")
        .and_then(|c| c.get("patches-file"))
        .and_then(Value::as_str)
        .or_else(|| extra.get("patches-file").and_then(Value::as_str))
        .map(str::to_string)
}

/// Flatten a `{ "vendor/pkg": <entries>, … }` map into a `Vec<Patch>`.
fn flatten_patch_map(map: &Value) -> Result<Vec<Patch>> {
    let obj = map
        .as_object()
        .ok_or_else(|| eyre::eyre!("`patches` must be an object keyed by package name"))?;
    let mut out = Vec::new();
    for (target, value) in obj {
        if !target.contains('/') {
            bail!("patch target `{target}` is not a `vendor/package` name");
        }
        out.extend(parse_target_patches(target, value)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn inline_patches_compact_and_expanded() {
        let cj = json!({
            "extra": {
                "patches": {
                    "vendor/a": { "Fix A": "patches/a.patch" },
                    "vendor/b": [ { "description": "Fix B", "url": "https://x/b.patch", "sha256": "ff" } ]
                }
            }
        });
        let dir = tempdir().unwrap();
        let mut patches = resolve_root(&cj, dir.path()).unwrap();
        patches.sort_by(|a, b| a.target.cmp(&b.target));
        assert_eq!(patches.len(), 2);
        assert_eq!(patches[0].target, "vendor/a");
        assert_eq!(patches[1].target, "vendor/b");
        assert_eq!(patches[1].sha256.as_deref(), Some("ff"));
    }

    #[test]
    fn inline_patches_bare_string_array() {
        // The exact shape from issue #422: a plain array of patch paths, as
        // accepted by cweagans/composer-patches. Must not error.
        let cj = json!({
            "extra": {
                "patches": {
                    "amasty/base": [ "patches/amasty/base/fix.patch" ]
                }
            }
        });
        let dir = tempdir().unwrap();
        let patches = resolve_root(&cj, dir.path()).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].target, "amasty/base");
        assert_eq!(
            patches[0].source,
            PatchSource::Local("patches/amasty/base/fix.patch".into())
        );
    }

    #[test]
    fn inline_wins_over_file() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("patches.json"),
            r#"{"patches":{"vendor/fromfile":{"X":"x.patch"}}}"#,
        )
        .unwrap();
        let cj = json!({
            "extra": {
                "patches": { "vendor/inline": { "Y": "y.patch" } },
                "patches-file": "patches.json"
            }
        });
        let patches = resolve_root(&cj, dir.path()).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].target, "vendor/inline");
    }

    #[test]
    fn external_file_v1_key() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("composer.patches.json"),
            r#"{"patches":{"vendor/pkg":{"Desc":"patches/p.patch"}}}"#,
        )
        .unwrap();
        let cj = json!({ "extra": { "patches-file": "composer.patches.json" } });
        let patches = resolve_root(&cj, dir.path()).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].target, "vendor/pkg");
        assert_eq!(patches[0].description, "Desc");
    }

    #[test]
    fn external_file_v2_namespaced_key_preferred() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("v2.json"),
            r#"{"patches":{"vendor/v2":{"D":"p.patch"}}}"#,
        )
        .unwrap();
        let cj = json!({
            "extra": {
                "patches-file": "missing-v1.json",
                "composer-patches": { "patches-file": "v2.json" }
            }
        });
        let patches = resolve_root(&cj, dir.path()).unwrap();
        assert_eq!(patches[0].target, "vendor/v2");
    }

    #[test]
    fn no_patches_is_empty() {
        let dir = tempdir().unwrap();
        let cj = json!({ "name": "acme/app" });
        assert!(resolve_root(&cj, dir.path()).unwrap().is_empty());
    }

    #[test]
    fn bad_target_name_errors() {
        let dir = tempdir().unwrap();
        let cj = json!({ "extra": { "patches": { "notapackage": { "x": "y.patch" } } } });
        assert!(resolve_root(&cj, dir.path()).is_err());
    }

    #[test]
    fn patches_dir_infers_target_and_ignores_non_patch_files() {
        let dir = tempdir().unwrap();
        let pdir = dir.path().join("patches");
        fs::create_dir_all(&pdir).unwrap();
        fs::write(
            pdir.join("fix.patch"),
            "--- a/vendor/acme/widget/src/W.php\n+++ b/vendor/acme/widget/src/W.php\n@@ -1 +1 @@\n-a\n+b\n",
        )
        .unwrap();
        // Non-.patch files are ignored.
        fs::write(pdir.join("README.md"), "notes").unwrap();
        fs::write(pdir.join("orig.diff"), "ignored").unwrap();

        let install_paths = vec![("acme/widget".to_string(), "vendor/acme/widget".to_string())];
        let patches = resolve_patches_dir(&pdir, &install_paths, &[]).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].target, "acme/widget");
        assert_eq!(patches[0].description, "fix.patch");
        assert_eq!(patches[0].depth, crate::DepthSpec::Fixed(4));
    }

    #[test]
    fn patches_dir_infers_root_patch_spanning_packages() {
        let dir = tempdir().unwrap();
        let pdir = dir.path().join("patches");
        fs::create_dir_all(&pdir).unwrap();
        // A top-level patch touching two packages via project-root paths.
        fs::write(
            pdir.join("perf.patch"),
            "--- a/vendor/acme/one/A.php\n+++ b/vendor/acme/one/A.php\n@@ -1 +1 @@\n-a\n+b\n\
             --- a/vendor/acme/two/B.php\n+++ b/vendor/acme/two/B.php\n@@ -1 +1 @@\n-c\n+d\n",
        )
        .unwrap();
        let install_paths = vec![
            ("acme/one".to_string(), "vendor/acme/one".to_string()),
            ("acme/two".to_string(), "vendor/acme/two".to_string()),
        ];
        let patches = resolve_patches_dir(&pdir, &install_paths, &[]).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(
            patches[0].scope,
            crate::model::PatchScope::Root {
                packages: vec!["acme/one".into(), "acme/two".into()]
            }
        );
        // Strip only the `a/` prefix and apply at the project root.
        assert_eq!(patches[0].depth, crate::DepthSpec::Fixed(1));
    }

    #[test]
    fn patches_dir_unresolvable_file_errors_with_name() {
        let dir = tempdir().unwrap();
        let pdir = dir.path().join("patches");
        fs::create_dir_all(&pdir).unwrap();
        fs::write(
            pdir.join("local.patch"),
            "--- a/Model/Foo.php\n+++ b/Model/Foo.php\n@@ -1 +1 @@\n-a\n+b\n",
        )
        .unwrap();
        let err = resolve_patches_dir(&pdir, &[], &[]).unwrap_err();
        assert!(format!("{err:#}").contains("local.patch"), "{err:#}");
    }

    #[test]
    fn patches_dir_skips_declared_files() {
        let dir = tempdir().unwrap();
        let pdir = dir.path().join("patches");
        fs::create_dir_all(&pdir).unwrap();
        // Package-relative headers: inference would hard-error on this file,
        // but it is declared explicitly, so the scan must skip it entirely.
        fs::write(
            pdir.join("declared.patch"),
            "--- a/Model/Foo.php\n+++ b/Model/Foo.php\n@@ -1 +1 @@\n-a\n+b\n",
        )
        .unwrap();
        fs::write(
            pdir.join("fix.patch"),
            "--- a/vendor/acme/widget/src/W.php\n+++ b/vendor/acme/widget/src/W.php\n@@ -1 +1 @@\n-a\n+b\n",
        )
        .unwrap();
        let install_paths = vec![("acme/widget".to_string(), "vendor/acme/widget".to_string())];
        // A non-canonical exclude path must still match (canonical compare).
        let declared = pdir.join("..").join("patches").join("declared.patch");
        let patches = resolve_patches_dir(&pdir, &install_paths, &[declared]).unwrap();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].description, "fix.patch");
    }

    #[test]
    fn missing_patches_dir_is_empty() {
        let dir = tempdir().unwrap();
        assert!(resolve_patches_dir(&dir.path().join("nope"), &[], &[]).unwrap().is_empty());
    }
}
