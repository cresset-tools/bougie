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
        // Composer-style footer: "Generated <mode> containing N classes".
        .stdout(contains("Generated autoload files containing"));

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
        // The "(authoritative)" qualifier in the Composer-style footer
        // is what tells us `--classmap-authoritative` propagated.
        .stdout(contains("(authoritative)"));

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
        .success();

    // APCu state isn't part of the Composer-style stdout footer, so
    // the propagation test is whether the emitted autoload_real.php
    // carries the setApcuPrefix call with our explicit prefix.
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

/// Report the effective `no_dev` a flag-less `dump-autoloader` resolves to,
/// after seeding `vendor/composer/installed.json` with the given dev mode.
fn no_dev_with_installed_dev(env: &TestEnv, installed_dev: bool, extra_arg: Option<&str>) -> bool {
    let work = stage("classmap-single");
    let composer_dir = work.path().join("vendor/composer");
    std::fs::create_dir_all(&composer_dir).unwrap();
    std::fs::write(
        composer_dir.join("installed.json"),
        format!(r#"{{ "packages": [], "dev": {installed_dev} }}"#),
    )
    .unwrap();

    let mut cmd = env.bougie();
    cmd.arg("composer").arg("dump-autoloader");
    if let Some(a) = extra_arg {
        cmd.arg(a);
    }
    let assert = cmd
        .arg("--format")
        .arg("json-v1")
        .current_dir(work.path())
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    v["no_dev"].as_bool().expect("no_dev bool in json-v1 output")
}

#[test]
fn cli_inherits_dev_mode_from_installed_json() {
    // Issue #499: a flag-less dump inherits the installed tree's dev mode.
    let env = TestEnv::new();
    // Installed --no-dev (dev:false) → the dump excludes dev.
    assert!(
        no_dev_with_installed_dev(&env, false, None),
        "flag-less dump on a --no-dev tree must resolve no_dev=true"
    );
    // Installed with dev (dev:true) → the dump includes dev.
    assert!(
        !no_dev_with_installed_dev(&env, true, None),
        "flag-less dump on a dev tree must resolve no_dev=false"
    );
}

#[test]
fn cli_explicit_dev_flag_overrides_installed_state() {
    let env = TestEnv::new();
    // `--dev` forces dev on even though installed.json recorded dev:false.
    assert!(
        !no_dev_with_installed_dev(&env, false, Some("--dev")),
        "--dev must override the installed dev:false"
    );
    // `--no-dev` forces dev off even though installed.json recorded dev:true.
    assert!(
        no_dev_with_installed_dev(&env, true, Some("--no-dev")),
        "--no-dev must override the installed dev:true"
    );
}
