//! End-to-end `bougie composer dump-autoloader` against the byte-
//! equivalence fixtures committed under
//! `crates/bougie-autoloader/tests/fixtures/`. The library-level
//! harness already proves byte-equivalence per fixture; this file
//! proves the CLI dispatches into it correctly (flag parsing,
//! `--working-dir`, error path on a missing composer.json, and the
//! `dump-autoload` alias for Composer muscle-memory).
//!
//! We copy fixture inputs into a tempdir per case so the committed
//! tree is never mutated. We don't diff against `expected/` here —
//! the autoloader crate already does that exhaustively. The CLI test
//! just confirms a non-empty `vendor/composer/autoload_real.php`
//! lands in the right place, plus a few targeted asserts that prove
//! flag plumbing.

use std::path::PathBuf;

use predicates::str::contains;

mod common;
use common::TestEnv;

fn fixture_input(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("bougie-autoloader")
        .join("tests")
        .join("fixtures")
        .join(name)
        .join("input")
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn stage(name: &str) -> tempfile::TempDir {
    let td = tempfile::TempDir::new().expect("tempdir");
    copy_dir(&fixture_input(name), td.path()).expect("copy fixture input");
    td
}

#[test]
fn cli_runs_in_cwd_by_default_and_emits_real_file() {
    let env = TestEnv::new();
    let work = stage("classmap-single");
    env.bougie()
        .arg("composer")
        .arg("dump-autoloader")
        .current_dir(work.path())
        .assert()
        .success()
        .stdout(contains("wrote vendor/composer/autoload_*.php"));

    let real = work.path().join("vendor/composer/autoload_real.php");
    assert!(real.is_file(), "autoload_real.php should land in cwd's vendor/composer/");
    let bytes = std::fs::read(&real).unwrap();
    assert!(!bytes.is_empty());
    assert!(
        std::str::from_utf8(&bytes).unwrap().contains("getLoader"),
        "autoload_real.php should contain the getLoader entry point"
    );
}

#[test]
fn cli_working_dir_flag_redirects_output() {
    let env = TestEnv::new();
    let work = stage("classmap-single");
    let unrelated = tempfile::TempDir::new().unwrap();
    env.bougie()
        .arg("composer")
        .arg("dump-autoloader")
        .arg("--working-dir")
        .arg(work.path())
        .current_dir(unrelated.path())
        .assert()
        .success();

    assert!(work.path().join("vendor/composer/autoload_real.php").is_file());
    assert!(
        !unrelated.path().join("vendor/composer/autoload_real.php").exists(),
        "no vendor/ should land in the cwd we ran from"
    );
}

#[test]
fn cli_classmap_authoritative_propagates_to_output() {
    let env = TestEnv::new();
    let work = stage("classmap-authoritative");
    // Fixture's bougie-flags carries classmap_authoritative=true.
    // Pass the same on the CLI here to mirror the byte-equivalence
    // contract.
    env.bougie()
        .arg("composer")
        .arg("dump-autoloader")
        .arg("--classmap-authoritative")
        .current_dir(work.path())
        .assert()
        .success()
        .stdout(contains("classmap-authoritative"));

    let real = std::fs::read_to_string(
        work.path().join("vendor/composer/autoload_real.php"),
    )
    .unwrap();
    assert!(
        real.contains("setClassMapAuthoritative(true)"),
        "autoload_real.php should carry the auth setter"
    );
}

#[test]
fn cli_apcu_prefix_implies_apcu_autoloader_and_propagates() {
    let env = TestEnv::new();
    let work = stage("apcu-autoloader");
    env.bougie()
        .arg("composer")
        .arg("dump-autoloader")
        .arg("--apcu-autoloader-prefix")
        .arg("fixture-apcu-prefix-zzz0")
        .current_dir(work.path())
        .assert()
        .success()
        .stdout(contains("apcu-autoloader"));

    let real = std::fs::read_to_string(
        work.path().join("vendor/composer/autoload_real.php"),
    )
    .unwrap();
    assert!(real.contains("setApcuPrefix('fixture-apcu-prefix-zzz0')"));
}

#[test]
fn cli_dump_autoload_alias_works_for_composer_muscle_memory() {
    let env = TestEnv::new();
    let work = stage("classmap-single");
    env.bougie()
        .arg("composer")
        .arg("dump-autoload") // note: no trailing -er; the alias
        .current_dir(work.path())
        .assert()
        .success();

    assert!(work.path().join("vendor/composer/autoload_real.php").is_file());
}

#[test]
fn cli_errors_when_composer_json_missing() {
    let env = TestEnv::new();
    let empty = tempfile::TempDir::new().unwrap();
    env.bougie()
        .arg("composer")
        .arg("dump-autoloader")
        .current_dir(empty.path())
        .assert()
        .failure()
        .stderr(contains("composer.json not found"));
}

#[test]
fn cli_json_output_carries_flag_state() {
    let env = TestEnv::new();
    let work = stage("classmap-single");
    let assert = env
        .bougie()
        .arg("composer")
        .arg("dump-autoloader")
        .arg("--no-dev")
        .arg("--format")
        .arg("json-v1")
        .current_dir(work.path())
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["no_dev"], true);
    assert_eq!(v["optimize"], false);
    assert_eq!(v["apcu_autoloader"], false);
}
