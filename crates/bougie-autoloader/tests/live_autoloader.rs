//! Live-autoloader API tests.
//!
//! `byte_equivalence.rs` is the full-fidelity Composer-parity harness;
//! it runs the public `dump_autoload` against every fixture. This file
//! exercises the [`Autoloader`] API directly: bootstrap state, the
//! `apply_*` patch flows, and the `user_code_roots` helper the server
//! uses to arm its filesystem watcher.
//!
//! The patch flow's correctness contract is "bootstrap-after-edit ==
//! bootstrap + `apply_changed_path(edit)` re-emitted". Every mutation
//! test checks that equivalence against a fresh-bootstrap baseline so
//! drift cannot hide behind the partial-update path.

use bougie_autoloader::{user_code_roots, Autoloader, DumpRequest};
use std::path::{Path, PathBuf};

const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn req(project: &Path, optimize: bool) -> DumpRequest<'_> {
    DumpRequest {
        project_root: project,
        optimize,
        classmap_authoritative: false,
        no_dev: false,
        apcu_autoloader: false,
        apcu_prefix: None,
        autoloader_suffix: None,
    }
}

/// Equivalence anchor: bootstrap an autoloader and emit; a *fresh*
/// bootstrap from the same project state must produce the same bytes.
/// Establishes that two `Autoloader` instances built from the same
/// inputs converge on the same output — a precondition for the
/// patch-flow tests below, which compare against a fresh-bootstrap
/// baseline.
#[test]
fn bootstrap_is_deterministic_against_fresh_state() {
    let fx = fixture("psr4-optimize");
    let project_a = copy_input_to_tempdir(&fx).unwrap();
    let project_b = copy_input_to_tempdir(&fx).unwrap();

    Autoloader::bootstrap(&req(project_a.path(), true))
        .unwrap()
        .emit()
        .unwrap();
    Autoloader::bootstrap(&req(project_b.path(), true))
        .unwrap()
        .emit()
        .unwrap();

    assert_classmap_matches(project_a.path(), project_b.path());
}

/// Adding a new PHP file under a watched `scan_root` with a fresh class
/// must produce the same classmap as a fresh bootstrap that sees the
/// file from the start.
#[test]
fn apply_changed_path_adds_new_class() {
    let fx = fixture("psr4-optimize");
    let live = copy_input_to_tempdir(&fx).unwrap();
    let baseline = copy_input_to_tempdir(&fx).unwrap();

    let new_rel = "vendor/acme/lib/src/NewThing.php";
    let new_body = "<?php\n\nnamespace Acme\\Lib;\n\nclass NewThing\n{\n}\n";

    // Bootstrap the live autoloader, then add the file, then patch.
    let mut loader = Autoloader::bootstrap(&req(live.path(), true)).unwrap();
    std::fs::write(live.path().join(new_rel), new_body).unwrap();
    let changed = loader.apply_changed_path(&live.path().join(new_rel)).unwrap();
    assert!(changed, "merged classmap should change when a new class lands");
    loader.emit().unwrap();

    // Baseline: write the file before bootstrap and emit fresh.
    std::fs::write(baseline.path().join(new_rel), new_body).unwrap();
    Autoloader::bootstrap(&req(baseline.path(), true))
        .unwrap()
        .emit()
        .unwrap();

    assert_classmap_matches(live.path(), baseline.path());
    // Sanity: the new class actually made it into the live classmap.
    let live_map = std::fs::read_to_string(
        live.path().join("vendor/composer/autoload_classmap.php"),
    )
    .unwrap();
    assert!(
        live_map.contains("'Acme\\\\Lib\\\\NewThing'"),
        "live classmap missing NewThing: {live_map}"
    );
}

/// A comment-only edit doesn't change the class list — `apply_changed_path`
/// should return `Ok(false)` and leave the merged classmap untouched.
#[test]
fn apply_changed_path_returns_false_on_comment_only_edit() {
    let fx = fixture("psr4-optimize");
    let project = copy_input_to_tempdir(&fx).unwrap();

    let mut loader = Autoloader::bootstrap(&req(project.path(), true)).unwrap();

    let target = project.path().join("vendor/acme/lib/src/Thing.php");
    let edited = "<?php\n\nnamespace Acme\\Lib;\n\n// a fresh comment that\n// adds no classes\nclass Thing\n{\n}\n";
    std::fs::write(&target, edited).unwrap();

    let changed = loader.apply_changed_path(&target).unwrap();
    assert!(!changed, "comment-only edit should not move the classmap");
}

/// Paths outside every task's `scan_root` are no-ops — the server can
/// route any FS event through the manager without pre-filtering.
#[test]
fn apply_changed_path_returns_false_for_out_of_scope_path() {
    let fx = fixture("psr4-optimize");
    let project = copy_input_to_tempdir(&fx).unwrap();

    let mut loader = Autoloader::bootstrap(&req(project.path(), true)).unwrap();

    let outside = project.path().join("docs").join("README.php");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, b"<?php class Doc {}").unwrap();

    let changed = loader.apply_changed_path(&outside).unwrap();
    assert!(!changed, "path outside any scan_root must not change the classmap");
}

/// Two files declare the same class; first-seen wins on bootstrap. If
/// the winner is deleted, the patch flow must re-resolve to the
/// surviving file — without per-file storage the autoloader would
/// silently keep the deleted file's path expression.
#[test]
fn apply_deleted_path_resolves_ambiguity() {
    let fx = fixture("psr4-shared-namespace");
    let project = copy_input_to_tempdir(&fx).unwrap();

    let mut loader = Autoloader::bootstrap(&req(project.path(), true)).unwrap();
    loader.emit().unwrap();

    let initial_map = std::fs::read_to_string(
        project.path().join("vendor/composer/autoload_classmap.php"),
    )
    .unwrap();
    assert!(
        initial_map.contains("'Shared\\\\Foo' => $vendorDir . '/acme/beta/src/Foo.php'"),
        "initial classmap expected beta to win — got:\n{initial_map}"
    );

    let beta = project.path().join("vendor/acme/beta/src/Foo.php");
    let changed = loader.apply_deleted_path(&beta).unwrap();
    assert!(changed, "deleting the winner should move the classmap");
    loader.emit().unwrap();

    let after_map = std::fs::read_to_string(
        project.path().join("vendor/composer/autoload_classmap.php"),
    )
    .unwrap();
    assert!(
        after_map.contains("'Shared\\\\Foo' => $vendorDir . '/acme/alpha/src/Foo.php'"),
        "after delete, alpha should win — got:\n{after_map}"
    );
}

/// Deleting a path the autoloader never saw is a no-op — important
/// because the watcher can fire spurious events for ignored files
/// (editor tempfiles, swp files, etc.).
#[test]
fn apply_deleted_path_is_idempotent_for_unknown_path() {
    let fx = fixture("psr4-optimize");
    let project = copy_input_to_tempdir(&fx).unwrap();

    let mut loader = Autoloader::bootstrap(&req(project.path(), true)).unwrap();
    let phantom = project.path().join("vendor/acme/lib/src/Never.php");
    let changed = loader.apply_deleted_path(&phantom).unwrap();
    assert!(!changed);
}

/// `user_code_roots` returns root-autoload directories plus path-repo
/// package `scan_roots`, canonicalized. `psr4-root-spans-vendor` is the
/// canonical fixture: root maps `App\\` → `.` and ships one path-repo
/// dep (`acme/sneak`) with no autoload — covers both code paths.
#[test]
fn user_code_roots_includes_root_and_path_repo_dirs() {
    let fx = fixture("psr4-root-spans-vendor");
    let project = copy_input_to_tempdir(&fx).unwrap();

    let roots = user_code_roots(&req(project.path(), false)).unwrap();

    let project_canonical = std::fs::canonicalize(project.path()).unwrap();
    let sneak_canonical =
        std::fs::canonicalize(project.path().join("vendor/acme/sneak")).unwrap();

    assert!(
        roots.contains(&project_canonical),
        "expected root scan_root {project_canonical:?} in {roots:?}"
    );
    assert!(
        roots.contains(&sneak_canonical),
        "expected path-repo scan_root {sneak_canonical:?} in {roots:?}"
    );
}

// ---------------------------------------------------------------- helpers

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(FIXTURES_DIR).join(name)
}

fn assert_classmap_matches(a: &Path, b: &Path) {
    let candidates = [
        "vendor/composer/autoload_classmap.php",
        "vendor/composer/autoload_static.php",
    ];
    for rel in candidates {
        let pa = a.join(rel);
        let pb = b.join(rel);
        let ba = std::fs::read(&pa).unwrap_or_else(|_| panic!("read {}", pa.display()));
        let bb = std::fs::read(&pb).unwrap_or_else(|_| panic!("read {}", pb.display()));
        assert!(ba == bb, 
            "{rel} differs between live-patched and fresh-bootstrap state\n--- live ---\n{}\n--- baseline ---\n{}",
            String::from_utf8_lossy(&ba),
            String::from_utf8_lossy(&bb),
        );
    }
}

fn copy_input_to_tempdir(fixture_dir: &Path) -> std::io::Result<TempDir> {
    let td = TempDir::new()?;
    copy_dir(&fixture_dir.join("input"), td.path())?;
    Ok(td)
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> std::io::Result<Self> {
        let base = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = base.join(format!(
            "bougie-autoloader-live-{nanos}-{}",
            std::process::id()
        ));
        std::fs::create_dir(&path)?;
        Ok(Self { path })
    }
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
