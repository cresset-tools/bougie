//! Phase 8 sanity: trivial commands that should work without an index.

mod common;

use common::TestEnv;
use predicates::str::contains;

#[test]
fn cache_clean_succeeds_when_cache_is_empty() {
    let env = TestEnv::new();
    env.bougie()
        .args(["cache", "clean"])
        .assert()
        .success()
        .stdout(contains("wiped"));
}

#[test]
fn cache_size_reports_zero_for_empty_env() {
    let env = TestEnv::new();
    env.bougie()
        .args(["cache", "size"])
        .assert()
        .success()
        .stdout(contains("total"))
        .stdout(contains("0 B"));
}

#[test]
fn ext_list_reads_composer_extensions() {
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"require":{"php":"^8.3","ext-xdebug":"*","ext-redis":"*"}}"#,
    )
    .unwrap();
    env.bougie()
        .current_dir(proj.path())
        .args(["ext", "list"])
        .assert()
        .success()
        .stdout(contains("ext-xdebug"))
        .stdout(contains("ext-redis"));
}

#[test]
fn ext_add_without_composer_fails_actionably() {
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    env.bougie()
        .current_dir(proj.path())
        .env("PATH", "/nonexistent")
        .args(["ext", "add", "xdebug"])
        .assert()
        .failure()
        .stderr(contains("composer is not on PATH"));
}

#[test]
fn php_pin_writes_to_bougie_toml() {
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(proj.path().join("bougie.toml"), "").unwrap();
    env.bougie()
        .current_dir(proj.path())
        .args(["php", "pin", "8.3.12"])
        .assert()
        .success();
    let body = std::fs::read_to_string(proj.path().join("bougie.toml")).unwrap();
    assert!(body.contains("8.3.12"), "got: {body}");
    assert!(body.contains("[php]"), "got: {body}");
}

#[test]
fn php_pin_writes_to_composer_extra_when_no_toml() {
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"require":{"php":"^8.3"}}"#,
    )
    .unwrap();
    env.bougie()
        .current_dir(proj.path())
        .args(["php", "pin", "8.3.12"])
        .assert()
        .success();
    let body = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["extra"]["bougie"]["php"]["version"], "8.3.12");
}

#[test]
fn self_update_is_stubbed() {
    let env = TestEnv::new();
    env.bougie()
        .args(["self", "update"])
        .assert()
        .failure()
        .stderr(contains("not yet available"));
}
