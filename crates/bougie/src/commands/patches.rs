//! Bridge between the install lifecycle and `bougie-patches`.
//!
//! Resolves the root patch set ([`bougie_patches::resolve_root`]), materializes
//! each patch (local files pass through; remote URLs download into the cache
//! with sha256 verify/TOFU), loads the applied fingerprints from
//! `patches.lock.json`, and assembles the [`PatchPlan`] the install
//! orchestrator consumes. Mirrors `commands/scripts.rs` as the lifecycle
//! bridge; the orchestrator stays FS/PHP-agnostic.

use std::collections::BTreeMap;
use std::path::Path;

use bougie_config::ProjectConfig;
use bougie_fetch::{ArchiveKind, BlobSpec, DownloadBar, Hash, default_client};
use bougie_patches::model::{FailureMode, PatchSource};
use bougie_patches::{
    MaterializedPatch, PatchPlan, content_sha256, lock, resolve_root,
};
use bougie_paths::Paths;
use eyre::{Result, WrapErr, bail};
use serde_json::Value;

/// Build the [`PatchPlan`] for an install, or `None` when patching is disabled
/// or the root declares no patches.
///
/// Enablement precedence: an explicit `--patches`/`--no-patches` CLI flag
/// wins, then `[patches] enable` (bougie config) / Composer's
/// `extra.enable-patching`, then the cweagans default of *on whenever patches
/// are declared*.
pub fn build_plan(
    paths: &Paths,
    project_root: &Path,
    project: &ProjectConfig,
    cli_flag: Option<bool>,
) -> Result<Option<PatchPlan>> {
    let composer_json = project_root.join("composer.json");
    let Ok(bytes) = std::fs::read(&composer_json) else {
        return Ok(None);
    };
    let value: Value =
        serde_json::from_slice(&bytes).wrap_err("parsing composer.json for patches")?;

    // Root sources, unioned: inline `extra.patches` / patches-file, plus the
    // zero-config `patches/` directory (target inferred from diff headers).
    let patches = resolve_all(project_root, project, &value)?;
    let applied = lock::read(project_root);

    // Enablement gate.
    let config_enable = project
        .bougie
        .patches
        .enable
        .or_else(|| value.pointer("/extra/enable-patching").and_then(Value::as_bool));
    let enabled = cli_flag.or(config_enable).unwrap_or(true);

    // When patching is off or nothing is declared, we still need a *cleanup*
    // plan if a previous run applied patches: the orchestrator must restore
    // those packages to pristine and clear `patches.lock.json`. With no
    // declared patches and no applied state there is genuinely nothing to do.
    if !enabled || patches.is_empty() {
        if applied.is_empty() {
            return Ok(None);
        }
        return Ok(Some(PatchPlan {
            patches: BTreeMap::new(),
            applied,
            failure_mode: FailureMode::SkipAndWarn,
            skip_report: skip_reporting(&value, project),
            write_lock: project.bougie.patches.write_lock.unwrap_or(false),
        }));
    }

    let failure_mode = if exit_on_failure(&value, project) {
        FailureMode::Abort
    } else {
        FailureMode::SkipAndWarn
    };
    let skip_report = skip_reporting(&value, project);

    // Materialize each patch, grouped by target package in declaration order.
    let client = default_client()?;
    let cache_dir = paths.cache().join("patches");
    let mut grouped: BTreeMap<String, Vec<MaterializedPatch>> = BTreeMap::new();
    for patch in patches {
        let materialized = materialize(&client, &cache_dir, project_root, &patch)?;
        grouped.entry(patch.target).or_default().push(materialized);
    }

    Ok(Some(PatchPlan {
        patches: grouped,
        applied,
        failure_mode,
        skip_report,
        write_lock: project.bougie.patches.write_lock.unwrap_or(false),
    }))
}

/// Resolve the root patch set without materializing (no network): inline
/// `extra.patches` / patches-file unioned with the zero-config `patches/`
/// directory, deduped. Used by `build_plan` and the read-only `patches list`
/// / `doctor` commands.
pub fn resolve_all(
    project_root: &Path,
    project: &ProjectConfig,
    composer_value: &Value,
) -> Result<Vec<bougie_patches::Patch>> {
    let mut patches = resolve_root(composer_value, project_root)?;
    let dir_name = patches_dir_name(composer_value, project);
    let install_paths = lock_install_paths(project_root, composer_value);
    let dir_patches =
        bougie_patches::resolve_patches_dir(&project_root.join(&dir_name), &install_paths)?;
    patches.extend(dir_patches);
    dedup_patches(&mut patches);
    Ok(patches)
}

/// A dependency's declared `extra.patches`, surfaced for `patches list` /
/// `doctor` / `import` (bougie never applies these automatically).
#[derive(Debug, Clone)]
pub struct DependencyPatches {
    /// The contributing dependency, `vendor/pkg`.
    pub dependency: String,
    /// Its version in the lock.
    pub version: String,
    /// The patches it declares (target package preserved in each `Patch`).
    pub patches: Vec<bougie_patches::Patch>,
}

/// Read every dependency-declared `extra.patches` from `composer.lock`. These
/// are *surfaced*, never auto-applied — the root-only policy. Malformed
/// entries are skipped silently (a dependency's bad config shouldn't break a
/// read-only listing).
pub fn dependency_patches(project_root: &Path) -> Vec<DependencyPatches> {
    let Ok(bytes) = std::fs::read(project_root.join("composer.lock")) else {
        return Vec::new();
    };
    let Ok(lock) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in ["packages", "packages-dev"] {
        let Some(arr) = lock.get(key).and_then(Value::as_array) else {
            continue;
        };
        for p in arr {
            let Some(dep) = p.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(map) = p.pointer("/extra/patches").and_then(Value::as_object) else {
                continue;
            };
            let version = p
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let mut patches = Vec::new();
            for (target, value) in map {
                if let Ok(parsed) = bougie_patches::parse_target_patches(target, value) {
                    patches.extend(parsed);
                }
            }
            if !patches.is_empty() {
                out.push(DependencyPatches {
                    dependency: dep.to_string(),
                    version,
                    patches,
                });
            }
        }
    }
    out
}

/// The `patches/` directory name: bougie `[patches] dir`, else Composer's
/// `extra.composer-patches.patches-dir`, else the default `patches`.
pub fn patches_dir_name(value: &Value, project: &ProjectConfig) -> String {
    if let Some(dir) = &project.bougie.patches.dir {
        return dir.clone();
    }
    value
        .pointer("/extra/composer-patches/patches-dir")
        .and_then(Value::as_str)
        .unwrap_or("patches")
        .to_string()
}

/// Compute each locked package's install directory (`vendor/<name>` or a
/// `composer/installers` remap), for `patches/` header inference. Reads
/// `composer.lock` directly; an absent/unparseable lock yields no paths (the
/// `patches/` dir then can't infer targets, which surfaces as a clear error).
pub fn lock_install_paths(project_root: &Path, composer_value: &Value) -> Vec<(String, String)> {
    let Ok(bytes) = std::fs::read(project_root.join("composer.lock")) else {
        return Vec::new();
    };
    let Ok(lock) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    let installer_paths = bougie_installers::InstallerPaths::parse(composer_value);
    let mut out = Vec::new();
    for key in ["packages", "packages-dev"] {
        let Some(arr) = lock.get(key).and_then(Value::as_array) else {
            continue;
        };
        for p in arr {
            let Some(name) = p.get("name").and_then(Value::as_str) else {
                continue;
            };
            let ptype = p.get("type").and_then(Value::as_str);
            let rel = bougie_installers::install_path(name, ptype, &installer_paths);
            out.push((name.to_string(), rel));
        }
    }
    out
}

/// Drop exact duplicate patches — same target + same source + same description
/// (a `patches/` file and an `extra.patches` entry pointing at the same patch).
fn dedup_patches(patches: &mut Vec<bougie_patches::Patch>) {
    let mut seen = std::collections::HashSet::new();
    patches.retain(|p| {
        let source = match &p.source {
            PatchSource::Local(path) => path.to_string_lossy().into_owned(),
            PatchSource::Remote(url) => url.clone(),
        };
        seen.insert((p.target.clone(), source, p.description.clone()))
    });
}

/// Materialize one resolved patch into a local file + content hash.
fn materialize(
    client: &reqwest::blocking::Client,
    cache_dir: &Path,
    project_root: &Path,
    patch: &bougie_patches::Patch,
) -> Result<MaterializedPatch> {
    let (local_path, content_sha256_hex, origin) = match &patch.source {
        PatchSource::Local(rel) => {
            let abs = if rel.is_absolute() {
                rel.clone()
            } else {
                project_root.join(rel)
            };
            let bytes = std::fs::read(&abs).wrap_err_with(|| {
                format!(
                    "patch file `{}` for `{}` not found",
                    abs.display(),
                    patch.target
                )
            })?;
            let sha = content_sha256(&bytes);
            if let Some(declared) = &patch.sha256
                && !declared.eq_ignore_ascii_case(&sha)
            {
                bail!(
                    "patch `{}` for `{}`: sha256 mismatch (declared {declared}, actual {sha})",
                    patch.description,
                    patch.target
                );
            }
            (abs, sha, rel.to_string_lossy().into_owned())
        }
        PatchSource::Remote(url) => {
            let dest = download_remote(client, cache_dir, url, patch.sha256.as_deref())?;
            let bytes = std::fs::read(&dest)
                .wrap_err_with(|| format!("reading downloaded patch `{}`", dest.display()))?;
            (dest, content_sha256(&bytes), url.clone())
        }
    };

    Ok(MaterializedPatch {
        description: patch.description.clone(),
        origin,
        local_path,
        content_sha256: content_sha256_hex,
        depth: patch.depth,
    })
}

/// Download a remote patch for `patches add` (no declared sha — TOFU),
/// returning the cached local path.
pub fn download_for_add(cache_dir: &Path, url: &str) -> Result<std::path::PathBuf> {
    let client = default_client()?;
    download_remote(&client, cache_dir, url, None)
}

/// Download a remote patch into the cache, verifying a declared sha256 or
/// trusting-on-first-use. Cache key is the declared sha (content-addressed) or
/// a hash of the URL when none is declared.
fn download_remote(
    client: &reqwest::blocking::Client,
    cache_dir: &Path,
    url: &str,
    declared_sha: Option<&str>,
) -> Result<std::path::PathBuf> {
    let partial = cache_dir.join(".partial");
    std::fs::create_dir_all(&partial)
        .wrap_err_with(|| format!("creating patch cache `{}`", cache_dir.display()))?;

    let key =
        declared_sha.map_or_else(|| content_sha256(url.as_bytes()), str::to_ascii_lowercase);
    let dest = cache_dir.join(format!("{key}.patch"));

    let spec = BlobSpec {
        url,
        hash: Hash::sha256(declared_sha.unwrap_or("")),
        partial_dir: &partial,
        dest: &dest,
        strip_prefix: "",
        archive: ArchiveKind::Zip, // ignored by fetch_file
        auth_header: None,
        auth_header_name: None,
    };
    bougie_fetch::fetch_file(client, &spec, &DownloadBar::hidden())
        .wrap_err_with(|| format!("downloading patch `{url}`"))?;
    Ok(dest)
}

/// Whether to abort the install on the first failed patch: bougie config wins,
/// then Composer's `extra.composer-exit-on-patch-failure`, then the
/// `COMPOSER_EXIT_ON_PATCH_FAILURE` env var.
fn exit_on_failure(value: &Value, project: &ProjectConfig) -> bool {
    if let Some(b) = project.bougie.patches.exit_on_failure {
        return b;
    }
    if let Some(b) = value
        .pointer("/extra/composer-exit-on-patch-failure")
        .and_then(Value::as_bool)
    {
        return b;
    }
    std::env::var_os("COMPOSER_EXIT_ON_PATCH_FAILURE").is_some()
}

/// Whether to suppress `PATCHES.txt`: bougie config, then Composer's
/// `extra.composer-patches-skip-reporting`, then `COMPOSER_PATCHES_SKIP_REPORTING`.
fn skip_reporting(value: &Value, project: &ProjectConfig) -> bool {
    if let Some(b) = project.bougie.patches.skip_report {
        return b;
    }
    if let Some(b) = value
        .pointer("/extra/composer-patches-skip-reporting")
        .and_then(Value::as_bool)
    {
        return b;
    }
    std::env::var_os("COMPOSER_PATCHES_SKIP_REPORTING").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bougie_config::{BougieConfig, PatchesConfig};
    use serde_json::json;

    fn project(enable: Option<bool>) -> ProjectConfig {
        ProjectConfig {
            composer: None,
            bougie: BougieConfig {
                patches: PatchesConfig {
                    enable,
                    ..Default::default()
                },
                ..Default::default()
            },
        }
    }

    #[test]
    fn exit_on_failure_precedence() {
        // bougie config wins.
        assert!(exit_on_failure(&Value::Null, &{
            let mut p = project(None);
            p.bougie.patches.exit_on_failure = Some(true);
            p
        }));
        // Composer extra key.
        assert!(exit_on_failure(
            &json!({"extra": {"composer-exit-on-patch-failure": true}}),
            &project(None)
        ));
        // Default off.
        assert!(!exit_on_failure(&Value::Null, &project(None)));
    }

    #[test]
    fn skip_reporting_from_composer_extra() {
        assert!(skip_reporting(
            &json!({"extra": {"composer-patches-skip-reporting": true}}),
            &project(None)
        ));
        assert!(!skip_reporting(&Value::Null, &project(None)));
    }

    #[test]
    fn build_plan_resolves_zero_config_patches_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("patches")).unwrap();
        std::fs::write(
            root.join("composer.json"),
            r#"{ "name": "acme/app", "require": { "acme/widget": "^1.0" } }"#,
        )
        .unwrap();
        std::fs::write(
            root.join("composer.lock"),
            r#"{ "content-hash": "x", "packages": [ { "name": "acme/widget", "version": "1.0.0", "type": "library" } ], "packages-dev": [] }"#,
        )
        .unwrap();
        // A project-root-relative patch — target inferred from the header.
        std::fs::write(
            root.join("patches/widget.patch"),
            "--- a/vendor/acme/widget/src/W.php\n+++ b/vendor/acme/widget/src/W.php\n@@ -1 +1 @@\n-a\n+b\n",
        )
        .unwrap();

        let paths = Paths::new(tmp.path().join("home"), tmp.path().join("cache"));
        let plan = build_plan(&paths, root, &project(None), None)
            .unwrap()
            .expect("a plan with the inferred patch");
        let widget = plan.patches.get("acme/widget").expect("inferred target");
        assert_eq!(widget.len(), 1);
        assert_eq!(widget[0].description, "widget.patch");
        // a/(1) + vendor/acme/widget(3) = depth 4.
        assert_eq!(
            widget[0].depth,
            bougie_patches::model::DepthSpec::Fixed(4)
        );
    }

    #[test]
    fn build_plan_none_when_no_patches_anywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("composer.json"), r#"{ "name": "acme/app" }"#).unwrap();
        let paths = Paths::new(tmp.path().join("home"), tmp.path().join("cache"));
        assert!(build_plan(&paths, root, &project(None), None).unwrap().is_none());
    }
}
