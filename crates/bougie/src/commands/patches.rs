//! Bridge between the install lifecycle and `composer-patches`.
//!
//! [`composer_patches::build_plan`] resolves the root patch set, materializes
//! each patch and loads the applied fingerprints from `patches.lock.json`;
//! magebuild builds its plan with the same function. This module maps bougie's
//! `[patches]` config and `--patches` flag onto its [`PlanOptions`], downloads
//! remote patches through `bougie-fetch`, and surfaces the patches dependencies
//! declare (never applied). Mirrors `commands/scripts.rs` as the lifecycle
//! bridge; the orchestrator stays FS/PHP-agnostic.

use std::path::{Path, PathBuf};

use bougie_config::ProjectConfig;
use bougie_fetch::{ArchiveKind, BlobSpec, DownloadBar, Hash, default_client};
use bougie_paths::Paths;
use composer_patches::{PatchFetcher, PatchPlan, PlanOptions, content_sha256};
use eyre::{Result, WrapErr};
use serde_json::Value;

pub use composer_patches::lock_install_paths;

/// Build the [`PatchPlan`] for an install, or `None` when patching is disabled
/// or the root declares no patches (and none were applied before).
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
    let fetcher = BougieFetcher {
        client: default_client()?,
    };
    composer_patches::build_plan(
        project_root,
        &plan_options(paths, project, cli_flag),
        &fetcher,
    )
}

/// bougie's `[patches]` config as [`PlanOptions`]: a setting it leaves unset
/// falls through to `composer.json` `extra`, then the `COMPOSER_*` env var.
fn plan_options(paths: &Paths, project: &ProjectConfig, cli_flag: Option<bool>) -> PlanOptions {
    let config = &project.bougie.patches;
    PlanOptions {
        enable: cli_flag.or(config.enable),
        exit_on_failure: config.exit_on_failure,
        skip_report: config.skip_report,
        write_lock: config.write_lock.unwrap_or(false),
        patches_dir: config.dir.clone(),
        cache_dir: Some(paths.cache().join("patches")),
    }
}

/// Resolve the root patch set without materializing (no network): inline
/// `extra.patches` / patches-file unioned with the zero-config `patches/`
/// directory, deduped. Used by the read-only `patches list` / `doctor`
/// commands.
pub fn resolve_all(
    project_root: &Path,
    project: &ProjectConfig,
    composer_value: &Value,
) -> Result<Vec<composer_patches::Patch>> {
    composer_patches::resolve_all(
        project_root,
        composer_value,
        project.bougie.patches.dir.as_deref(),
    )
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
    pub patches: Vec<composer_patches::Patch>,
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
                if let Ok(parsed) = composer_patches::parse_target_patches(target, value) {
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
    composer_patches::patches_dir_name(value, project.bougie.patches.dir.as_deref())
}

/// Download a remote patch for `patches add` (no declared sha — trusted on
/// first use), returning the cached local path. Same cache key as
/// [`build_plan`] uses for an undeclared patch, so the install that follows
/// finds it there.
pub fn download_for_add(cache_dir: &Path, url: &str) -> Result<PathBuf> {
    let dest = cache_dir.join(format!("{}.patch", content_sha256(url.as_bytes())));
    if !dest.exists() {
        std::fs::create_dir_all(cache_dir)
            .wrap_err_with(|| format!("creating patch cache `{}`", cache_dir.display()))?;
        BougieFetcher {
            client: default_client()?,
        }
        .fetch(url, &dest)
        .wrap_err_with(|| format!("downloading patch `{url}`"))?;
    }
    Ok(dest)
}

/// Downloads remote patches with `bougie-fetch`: retries, and an atomic write
/// through a `.partial` directory beside the cache. `composer-patches` checks
/// a declared sha256 afterwards.
struct BougieFetcher {
    client: reqwest::blocking::Client,
}

impl PatchFetcher for BougieFetcher {
    fn fetch(&self, url: &str, dest: &Path) -> Result<()> {
        let partial = dest.parent().unwrap_or(Path::new(".")).join(".partial");
        std::fs::create_dir_all(&partial)
            .wrap_err_with(|| format!("creating `{}`", partial.display()))?;
        let spec = BlobSpec {
            url,
            hash: Hash::sha256(""),
            partial_dir: &partial,
            dest,
            strip_prefix: "",
            archive: ArchiveKind::Zip, // ignored by fetch_file
            auth_header: None,
            auth_header_name: None,
        };
        bougie_fetch::fetch_file(&self.client, &spec, &DownloadBar::hidden())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bougie_config::{BougieConfig, PatchesConfig};

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
    fn plan_options_take_bougie_config_and_the_cli_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("home"), tmp.path().join("cache"));

        // Unset config defers every setting to composer.json and the env.
        let opts = plan_options(&paths, &project(None), None);
        assert_eq!(opts.enable, None);
        assert_eq!(opts.exit_on_failure, None);
        assert_eq!(opts.skip_report, None);
        assert!(!opts.write_lock);
        assert_eq!(opts.patches_dir, None);
        assert_eq!(opts.cache_dir, Some(paths.cache().join("patches")));

        let mut p = project(Some(true));
        p.bougie.patches.exit_on_failure = Some(true);
        p.bougie.patches.skip_report = Some(false);
        p.bougie.patches.write_lock = Some(true);
        p.bougie.patches.dir = Some("fixes".into());
        let opts = plan_options(&paths, &p, None);
        assert_eq!(opts.enable, Some(true));
        assert_eq!(opts.exit_on_failure, Some(true));
        assert_eq!(opts.skip_report, Some(false));
        assert!(opts.write_lock);
        assert_eq!(opts.patches_dir.as_deref(), Some("fixes"));

        // --no-patches beats `[patches] enable = true`.
        assert_eq!(plan_options(&paths, &p, Some(false)).enable, Some(false));
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
            composer_patches::model::DepthSpec::Fixed(4)
        );
    }

    #[test]
    fn build_plan_declared_entry_wins_over_patches_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("patches")).unwrap();
        // Declared in extra.patches AND sitting in the scanned patches/
        // dir — package-relative headers, which the zero-config scan alone
        // could not even infer a target for.
        std::fs::write(
            root.join("composer.json"),
            r#"{ "name": "acme/app", "require": { "acme/widget": "^1.0" },
                 "extra": { "patches": { "acme/widget": { "Fix W": "patches/widget.patch" } } } }"#,
        )
        .unwrap();
        std::fs::write(
            root.join("composer.lock"),
            r#"{ "content-hash": "x", "packages": [ { "name": "acme/widget", "version": "1.0.0", "type": "library" } ], "packages-dev": [] }"#,
        )
        .unwrap();
        std::fs::write(
            root.join("patches/widget.patch"),
            "--- a/src/W.php\n+++ b/src/W.php\n@@ -1 +1 @@\n-a\n+b\n",
        )
        .unwrap();

        let paths = Paths::new(tmp.path().join("home"), tmp.path().join("cache"));
        let plan = build_plan(&paths, root, &project(None), None)
            .unwrap()
            .expect("a plan with the declared patch");
        let widget = plan.patches.get("acme/widget").expect("declared target");
        assert_eq!(widget.len(), 1, "declared once, not doubled by the scan");
        assert_eq!(widget[0].description, "Fix W");
    }

    #[test]
    fn build_plan_none_when_no_patches_anywhere() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("composer.json"), r#"{ "name": "acme/app" }"#).unwrap();
        let paths = Paths::new(tmp.path().join("home"), tmp.path().join("cache"));
        assert!(
            build_plan(&paths, root, &project(None), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn build_plan_defers_vendor_internal_missing_patch() {
        // Issue #421: the patch source is shipped by a Composer package and
        // referenced under vendor/, but vendor/ was wiped. build_plan must
        // defer it (not hard-error) so the owning package can install first.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("composer.json"),
            r#"{ "name": "acme/app",
                 "require": { "acme/widget": "^1.0", "acme/patches-repo": "^1.0" },
                 "extra": { "patches": { "acme/widget":
                     { "Fix from vendor": "vendor/acme/patches-repo/fix.patch" } } } }"#,
        )
        .unwrap();
        std::fs::write(
            root.join("composer.lock"),
            r#"{ "content-hash": "x", "packages": [
                { "name": "acme/widget", "version": "1.0.0", "type": "library" },
                { "name": "acme/patches-repo", "version": "1.0.0", "type": "library" }
            ], "packages-dev": [] }"#,
        )
        .unwrap();

        let paths = Paths::new(root.join("home"), root.join("cache"));
        let plan = build_plan(&paths, root, &project(None), None)
            .unwrap()
            .expect("a plan with the deferred patch");
        assert!(plan.patches.is_empty(), "must not materialize eagerly");
        assert_eq!(plan.deferred.len(), 1);
        assert_eq!(plan.deferred[0].target, "acme/widget");
        assert_eq!(
            plan.deferred[0].origin,
            "vendor/acme/patches-repo/fix.patch"
        );
    }

    #[test]
    fn build_plan_errors_on_missing_patch_outside_vendor() {
        // A missing *project* patch (typo, wrong path) is NOT under vendor/ and
        // must still fail fast, before any install — not silently deferred.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("composer.json"),
            r#"{ "name": "acme/app", "require": { "acme/widget": "^1.0" },
                 "extra": { "patches": { "acme/widget": { "Fix": "patches/typo.patch" } } } }"#,
        )
        .unwrap();
        std::fs::write(
            root.join("composer.lock"),
            r#"{ "content-hash": "x", "packages": [ { "name": "acme/widget", "version": "1.0.0", "type": "library" } ], "packages-dev": [] }"#,
        )
        .unwrap();
        let paths = Paths::new(root.join("home"), root.join("cache"));
        assert!(build_plan(&paths, root, &project(None), None).is_err());
    }
}
