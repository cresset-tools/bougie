//! Phase 3: `bougie init` and `bougie init --toml`.

mod common;

use common::TestEnv;
use predicates::str::contains;
use tempfile::TempDir;

fn project_dir() -> TempDir {
    TempDir::new().expect("project tempdir")
}

#[test]
fn init_creates_composer_and_bougie_skeleton() {
    let env = TestEnv::new();
    let proj = project_dir();
    env.bougie()
        .current_dir(proj.path())
        .arg("init")
        .assert()
        .success()
        .stdout(contains("composer.json"))
        .stdout(contains(".bougie/conf.d"));

    assert!(proj.path().join("composer.json").is_file());
    assert!(proj.path().join(".bougie/conf.d").is_dir());
    assert!(proj.path().join(".bougie/bin").is_dir());
    assert!(proj.path().join(".bougie/state").is_dir());
    assert!(proj.path().join(".bougie/.gitignore").is_file());
    assert!(!proj.path().join("bougie.toml").exists());
    assert!(!proj.path().join(".php-version").exists());
}

#[test]
fn init_does_not_overwrite_existing_composer() {
    let env = TestEnv::new();
    let proj = project_dir();
    let custom = r#"{"name":"acme/example","require":{"php":"^8.2"}}"#;
    std::fs::write(proj.path().join("composer.json"), custom).unwrap();

    env.bougie()
        .current_dir(proj.path())
        .arg("init")
        .assert()
        .success()
        .stdout(contains("kept"));

    let read = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert_eq!(read, custom);
}

#[test]
fn init_toml_creates_bougie_toml() {
    let env = TestEnv::new();
    let proj = project_dir();
    env.bougie()
        .current_dir(proj.path())
        .args(["init", "--toml"])
        .assert()
        .success();
    let toml = proj.path().join("bougie.toml");
    assert!(toml.is_file());
    let text = std::fs::read_to_string(&toml).unwrap();
    assert!(text.contains("[php]"));
    assert!(text.contains("[extensions]"));
}

#[test]
fn init_default_composer_has_require_php() {
    let env = TestEnv::new();
    let proj = project_dir();
    env.bougie()
        .current_dir(proj.path())
        .arg("init")
        .assert()
        .success();
    let text = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let php = v["require"]["php"].as_str().unwrap();
    assert!(php.starts_with('^'));
}

#[test]
fn init_json_v1_envelope() {
    let env = TestEnv::new();
    let proj = project_dir();
    let out = env
        .bougie()
        .current_dir(proj.path())
        .args(["init", "--format", "json-v1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert!(v["created"].is_array());
}

#[test]
fn init_is_idempotent() {
    let env = TestEnv::new();
    let proj = project_dir();
    env.bougie().current_dir(proj.path()).arg("init").assert().success();
    let first = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();

    env.bougie()
        .current_dir(proj.path())
        .arg("init")
        .assert()
        .success()
        .stdout(contains("kept"));
    let second = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert_eq!(first, second);
}
