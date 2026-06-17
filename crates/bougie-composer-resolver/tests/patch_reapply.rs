//! Phase B acceptance: the re-application invariant, end to end.
//!
//! Drives `install_from_lock_with_patches` over a real local-artifact zip and
//! a `patches/` file, walking the full state matrix:
//!
//! 1. fresh install with a patch → file is patched;
//! 2. re-run, nothing changed → no re-extract, no re-apply (idempotent);
//! 3. edit the patch (package version unchanged) → package re-extracted
//!    pristine and re-patched (the load-bearing case);
//! 4. remove the patch → package restored to pristine, lock entry dropped.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use bougie_composer_resolver::{InstallOptions, install_from_lock_with_patches};
use bougie_patches::model::{DepthSpec, FailureMode};
use bougie_patches::{MaterializedPatch, PatchPlan, content_sha256, lock};
use bougie_paths::Paths;
use tempfile::TempDir;

const PRISTINE: &str = "alpha\nbeta\ngamma\n";

fn paths_in(tmp: &Path) -> Paths {
    let home = tmp.join("home");
    let cache = tmp.join("cache");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    Paths::new(home, cache)
}

/// Build a Composer-style artifact zip wrapping `files` under `wrapper/`.
fn make_zip(dest: &Path, wrapper: &str, files: &[(&str, &str)]) {
    let f = std::fs::File::create(dest).unwrap();
    let mut zip = zip::ZipWriter::new(f);
    let opts: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (rel, content) in files {
        zip.start_file(format!("{wrapper}/{rel}"), opts).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

fn composer_json(with_patch: bool) -> String {
    let patches = if with_patch {
        r#", "extra": { "patches": { "acme/widget": { "Fix beta": "patches/fix.patch" } } }"#
    } else {
        ""
    };
    format!(
        r#"{{ "name": "acme/test", "require": {{ "acme/widget": "^1.0" }}{patches} }}"#
    )
}

fn write_lock(project_root: &Path, composer_json: &str, artifact: &Path) {
    let hash = bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [
                {{
                    "name": "acme/widget",
                    "version": "1.0.0",
                    "type": "library",
                    "dist": {{ "type": "zip", "url": "{}", "shasum": "" }}
                }}
            ],
            "packages-dev": []
        }}"#,
        artifact.display()
    );
    std::fs::write(project_root.join("composer.lock"), lock).unwrap();
}

/// Construct the plan the way the CLI bridge would: one patch for acme/widget,
/// reading the applied fingerprints from the on-disk lock.
fn plan_for(project_root: &Path, patch_file: &Path) -> PatchPlan {
    let bytes = std::fs::read(patch_file).unwrap();
    let mp = MaterializedPatch {
        description: "Fix beta".into(),
        origin: "patches/fix.patch".into(),
        local_path: patch_file.to_path_buf(),
        content_sha256: content_sha256(&bytes),
        depth: DepthSpec::Auto,
    };
    let mut patches = BTreeMap::new();
    patches.insert("acme/widget".to_string(), vec![mp]);
    PatchPlan {
        patches,
        applied: lock::read(project_root),
        failure_mode: FailureMode::Abort,
        skip_report: false,
        write_lock: false,
    }
}

/// The cleanup plan (no patches declared) — what the bridge returns once the
/// lock has entries but composer.json no longer declares patches.
fn cleanup_plan(project_root: &Path) -> PatchPlan {
    PatchPlan {
        patches: BTreeMap::new(),
        applied: lock::read(project_root),
        failure_mode: FailureMode::SkipAndWarn,
        skip_report: false,
        write_lock: false,
    }
}

#[test]
fn reapplication_state_matrix() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(proj.join("patches")).unwrap();

    let artifact = tmp.path().join("widget.zip");
    make_zip(&artifact, "widget-1.0.0", &[("src/Widget.php", PRISTINE)]);

    let widget_php = proj.join("vendor/acme/widget/src/Widget.php");
    let patches_txt = proj.join("vendor/acme/widget/PATCHES.txt");
    let patch_file = proj.join("patches/fix.patch");

    // ---- 1. fresh install with a patch -> patched.
    let cj = composer_json(true);
    std::fs::write(proj.join("composer.json"), &cj).unwrap();
    write_lock(&proj, &cj, &artifact);
    std::fs::write(
        &patch_file,
        "--- a/src/Widget.php\n+++ b/src/Widget.php\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n",
    )
    .unwrap();

    let s1 = install_from_lock_with_patches(
        &paths,
        &proj,
        InstallOptions::default(),
        None,
        Some(&plan_for(&proj, &patch_file)),
    )
    .unwrap();
    assert_eq!(s1.packages_installed, 1, "widget should be extracted");
    assert_eq!(std::fs::read_to_string(&widget_php).unwrap(), "alpha\nBETA\ngamma\n");
    assert!(patches_txt.exists(), "PATCHES.txt written");
    assert!(
        lock::read(&proj).contains_key("acme/widget"),
        "fingerprint recorded"
    );

    // ---- 2. re-run, nothing changed -> idempotent (no re-extract/re-apply).
    let s2 = install_from_lock_with_patches(
        &paths,
        &proj,
        InstallOptions::default(),
        None,
        Some(&plan_for(&proj, &patch_file)),
    )
    .unwrap();
    assert_eq!(s2.packages_installed, 0, "no re-extract when unchanged");
    assert_eq!(s2.packages_up_to_date, 1);
    assert_eq!(
        std::fs::read_to_string(&widget_php).unwrap(),
        "alpha\nBETA\ngamma\n",
        "still patched exactly once"
    );

    // ---- 3. edit the patch (version unchanged) -> re-extract + re-apply.
    std::fs::write(
        &patch_file,
        "--- a/src/Widget.php\n+++ b/src/Widget.php\n@@ -1,3 +1,3 @@\n alpha\n beta\n-gamma\n+GAMMA\n",
    )
    .unwrap();
    let s3 = install_from_lock_with_patches(
        &paths,
        &proj,
        InstallOptions::default(),
        None,
        Some(&plan_for(&proj, &patch_file)),
    )
    .unwrap();
    // The package is re-extracted (not skipped as up-to-date) — `packages_up_to_date`
    // is 0, and the tree is wiped + restored before the new patch applies, so the
    // step-1 BETA edit is gone and only the new GAMMA edit remains. (`packages_installed`
    // counts network downloads; the artifact zip is cached, so it reads 0.)
    assert_eq!(s3.packages_up_to_date, 0, "patch edit forces a re-extract, not a skip");
    assert_eq!(
        std::fs::read_to_string(&widget_php).unwrap(),
        "alpha\nbeta\nGAMMA\n",
        "beta restored to pristine, gamma now patched"
    );

    // ---- 4. remove the patch -> restore pristine, drop the lock entry.
    let cj = composer_json(false);
    std::fs::write(proj.join("composer.json"), &cj).unwrap();
    write_lock(&proj, &cj, &artifact);
    std::fs::remove_file(&patch_file).unwrap();

    let s4 = install_from_lock_with_patches(
        &paths,
        &proj,
        InstallOptions::default(),
        None,
        Some(&cleanup_plan(&proj)),
    )
    .unwrap();
    assert_eq!(s4.packages_up_to_date, 0, "removal forces a re-extract, not a skip");
    assert_eq!(
        std::fs::read_to_string(&widget_php).unwrap(),
        PRISTINE,
        "file restored to pristine"
    );
    assert!(!patches_txt.exists(), "PATCHES.txt removed with the pristine tree");
    assert!(
        !lock::read(&proj).contains_key("acme/widget"),
        "fingerprint dropped"
    );
}
