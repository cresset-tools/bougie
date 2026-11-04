//! `bougie repo add/remove`: edit the committed composer.json's
//! `repositories`, idempotently, keeping a lock's content-hash in step.

mod common;

use common::TestEnv;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

fn run(env: &TestEnv, dir: &Path, args: &[&str]) -> String {
    let out = env
        .bougie()
        .arg("repo")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run bougie");
    assert!(
        out.status.success(),
        "bougie repo {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn composer_json(dir: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(dir.join("composer.json")).unwrap()).unwrap()
}

#[test]
fn add_then_remove_a_repository() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    std::fs::write(proj.path().join("composer.json"), r#"{"name":"test/p"}"#).unwrap();

    let out = run(
        &env,
        proj.path(),
        &["add", "https://mageos.bougierepo.com/"],
    );
    assert!(out.contains("Added https://mageos.bougierepo.com"), "{out}");
    assert_eq!(
        composer_json(proj.path())["repositories"],
        serde_json::json!([{ "type": "composer", "url": "https://mageos.bougierepo.com" }])
    );

    // Again, with or without the slash: still one entry.
    let out = run(&env, proj.path(), &["add", "https://mageos.bougierepo.com"]);
    assert!(out.contains("already in composer.json"), "{out}");
    assert_eq!(
        composer_json(proj.path())["repositories"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let out = run(
        &env,
        proj.path(),
        &["remove", "https://mageos.bougierepo.com"],
    );
    assert!(out.contains("Removed"), "{out}");
    assert_eq!(
        composer_json(proj.path())["repositories"],
        serde_json::json!([])
    );
    let out = run(
        &env,
        proj.path(),
        &["remove", "https://mageos.bougierepo.com"],
    );
    assert!(out.contains("is not in composer.json"), "{out}");
}

#[test]
fn a_lock_keeps_a_matching_content_hash() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    std::fs::write(proj.path().join("composer.json"), r#"{"name":"test/p"}"#).unwrap();
    std::fs::write(
        proj.path().join("composer.lock"),
        r#"{"content-hash":"stale","packages":[],"packages-dev":[]}"#,
    )
    .unwrap();

    run(&env, proj.path(), &["add", "https://repo.example.org"]);
    let bytes = std::fs::read(proj.path().join("composer.json")).unwrap();
    let lock: Value =
        serde_json::from_slice(&std::fs::read(proj.path().join("composer.lock")).unwrap()).unwrap();
    assert_eq!(
        lock["content-hash"].as_str().unwrap(),
        bougie_composer::lockfile::content_hash(&bytes).unwrap()
    );
}

#[test]
fn refuses_outside_a_project_and_non_urls() {
    let env = TestEnv::new();
    let empty = TempDir::new().unwrap();
    let out = env
        .bougie()
        .args(["repo", "add", "https://repo.example.org"])
        .current_dir(empty.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no composer.json"));

    std::fs::write(empty.path().join("composer.json"), "{}").unwrap();
    let out = env
        .bougie()
        .args(["repo", "add", "repo.example.org"])
        .current_dir(empty.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not a URL"));
}
