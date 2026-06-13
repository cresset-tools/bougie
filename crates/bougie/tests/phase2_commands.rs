//! Phase 2: cache dir, php dir, self version, and shim dispatch.

mod common;

use common::TestEnv;
use predicates::str::{contains, starts_with};

#[test]
fn cache_dir_text() {
    let env = TestEnv::new();
    let out = env
        .bougie()
        .args(["cache", "dir"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let line = String::from_utf8(out).unwrap();
    assert_eq!(line.trim(), env.cache_path().to_str().unwrap());
}

#[test]
fn cache_dir_json_v1() {
    let env = TestEnv::new();
    let out = env
        .bougie()
        .args(["cache", "dir", "--format", "json-v1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["path"], env.cache_path().to_str().unwrap());
}

#[test]
fn php_dir_text() {
    let env = TestEnv::new();
    let expected = env.home_path().join("installs");
    env.bougie()
        .args(["php", "dir"])
        .assert()
        .success()
        .stdout(starts_with(expected.to_str().unwrap()));
}

#[test]
fn self_version_text_has_two_lines() {
    let env = TestEnv::new();
    env.bougie()
        .args(["self", "version"])
        .assert()
        .success()
        .stdout(contains("bougie "))
        .stdout(contains("trust ("));
}

#[test]
fn self_version_short_prints_bare_version() {
    let env = TestEnv::new();
    let out = env
        .bougie()
        .args(["self", "version", "--short"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let line = String::from_utf8(out).unwrap();
    assert_eq!(line.trim(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn self_version_json_v1_carries_schema_version() {
    let env = TestEnv::new();
    let out = env
        .bougie()
        .args(["self", "version", "--format", "json-v1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["bougie"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(v["bougie"]["trust"]["kind"].is_string());
    assert!(v["bougie"]["trust"]["detail"].is_string());
}

#[test]
fn shim_dispatch_via_argv0_symlink() {
    use std::os::unix::fs::symlink;

    let env = TestEnv::new();
    let bin = assert_cmd::cargo::cargo_bin("bougie");
    let dir = tempfile::TempDir::new().unwrap();
    // The shim lives at `<project>/vendor/bougie/bin/php`; the argv[0]
    // walk climbs four parents back to the (un-synced) project root.
    let bin_dir = dir.path().join("vendor").join("bougie").join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let php_link = bin_dir.join("php");
    symlink(&bin, &php_link).unwrap();

    let out = std::process::Command::new(&php_link)
        .env("BOUGIE_HOME", env.home_path())
        .env("BOUGIE_CACHE", env.cache_path())
        .arg("-v")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("not synced"),
        "stderr was: {stderr}"
    );
}
