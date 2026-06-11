//! Integration tests for the read-only inspection subcommands —
//! `bougie composer show` / `why` / `why-not`. These are offline: they
//! read a staged `composer.json` + `composer.lock` and never hit the
//! network (no `--latest`/`--outdated` here, which would).

use std::path::Path;
use tempfile::TempDir;

mod common;
use common::TestEnv;

fn stage(dir: &Path, composer_json: &str, composer_lock: &str) {
    std::fs::write(dir.join("composer.json"), composer_json).unwrap();
    std::fs::write(dir.join("composer.lock"), composer_lock).unwrap();
}

const COMPOSER_JSON: &str =
    r#"{"name":"test/proj","require":{"acme/lib":"^2.0","php":"^8.1"}}"#;
const LOCK: &str = r#"{
    "content-hash":"x",
    "packages":[
        {"name":"acme/lib","version":"2.3.0","description":"A lib","require":{"psr/log":"^3.0"}},
        {"name":"psr/log","version":"3.0.0","description":"Logging"}
    ],
    "packages-dev":[
        {"name":"phpunit/phpunit","version":"10.5.0","description":"Testing"}
    ]
}"#;

#[test]
fn show_lists_installed_packages() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    let out = env
        .bougie()
        .args(["composer", "show", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("acme/lib") && s.contains("2.3.0"), "{s}");
    assert!(s.contains("psr/log"), "{s}");
    // dev package shown without --no-dev.
    assert!(s.contains("phpunit/phpunit"), "{s}");
}

#[test]
fn show_no_dev_excludes_dev_packages() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    let out = env
        .bougie()
        .args(["composer", "show", "--no-dev", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("acme/lib"), "{s}");
    assert!(!s.contains("phpunit/phpunit"), "dev pkg should be hidden: {s}");
}

#[test]
fn show_direct_only_root_requires() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    let out = env
        .bougie()
        .args(["composer", "show", "--direct", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("acme/lib"), "{s}");
    // psr/log is transitive, not a direct require.
    assert!(!s.contains("psr/log"), "transitive dep should be excluded: {s}");
}

#[test]
fn show_tree_renders_hierarchy() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    let out = env
        .bougie()
        .args(["composer", "show", "--tree", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("test/proj"), "root line: {s}");
    assert!(s.contains("acme/lib ^2.0"), "direct dep: {s}");
    // Nested transitive under acme/lib.
    assert!(s.contains("psr/log ^3.0"), "nested dep: {s}");
    assert!(s.contains("└──") || s.contains("├──"), "box drawing: {s}");
}

#[test]
fn show_single_package_detail() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    let out = env
        .bougie()
        .args(["composer", "show", "acme/lib", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("name") && s.contains("acme/lib"), "{s}");
    assert!(s.contains("2.3.0"), "{s}");
    assert!(s.contains("psr/log"), "requires section: {s}");
}

#[test]
fn show_format_json_emits_structured_payload() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    let out = env
        .bougie()
        .args(["composer", "show", "--format", "json", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let rows = v.get("rows").and_then(|r| r.as_array()).expect("rows array");
    assert!(rows.iter().any(|r| r.get("name").and_then(|n| n.as_str()) == Some("acme/lib")));
}

#[test]
fn why_reports_dependents_including_root() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    // psr/log is required by acme/lib (transitive).
    let out = env
        .bougie()
        .args(["composer", "why", "psr/log", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("acme/lib"), "acme/lib requires psr/log: {s}");
    assert!(s.contains("^3.0"), "constraint shown: {s}");

    // acme/lib is required directly by the root project.
    let out2 = env
        .bougie()
        .args(["composer", "why", "acme/lib", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(s2.contains("test/proj"), "root should be a dependent: {s2}");
}

#[test]
fn why_none_for_unrequired_package() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    let out = env
        .bougie()
        .args(["composer", "why", "phpunit/phpunit", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    // phpunit is in the lock but nothing in the locked set requires it
    // (it's a root dev require, which this fixture's composer.json omits).
    assert!(s.contains("no installed package depending"), "{s}");
}

#[test]
fn why_not_reports_conflict() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    // acme/lib conflicts with psr/log >=3.0; psr/log is locked at 2.0.
    let lock = r#"{
        "content-hash":"x",
        "packages":[
            {"name":"acme/lib","version":"2.3.0","conflict":{"psr/log":">=3.0"}},
            {"name":"psr/log","version":"2.0.0"}
        ],
        "packages-dev":[]
    }"#;
    stage(proj.path(), r#"{"name":"test/proj","require":{"acme/lib":"^2.0"}}"#, lock);

    let out = env
        .bougie()
        .args(["composer", "why-not", "psr/log", "3.0.0", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("acme/lib"), "{s}");
    assert!(s.contains("conflicts with"), "{s}");
    assert!(s.contains(">=3.0"), "{s}");
}

#[test]
fn why_not_reports_require_excluding_version() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    // acme/lib requires psr/log ^2.0; testing whether 3.0.0 could go in
    // shows acme/lib's require as the prohibitor.
    let lock = r#"{
        "content-hash":"x",
        "packages":[
            {"name":"acme/lib","version":"2.3.0","require":{"psr/log":"^2.0"}},
            {"name":"psr/log","version":"2.5.0"}
        ],
        "packages-dev":[]
    }"#;
    stage(proj.path(), r#"{"name":"test/proj","require":{"acme/lib":"^2.0"}}"#, lock);

    let out = env
        .bougie()
        .args(["composer", "why-not", "psr/log", "3.0.0", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("acme/lib"), "{s}");
    assert!(s.contains("requires"), "{s}");
    assert!(s.contains("^2.0"), "{s}");
}

#[test]
fn show_without_lock_errors() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    std::fs::write(proj.path().join("composer.json"), COMPOSER_JSON).unwrap();

    let out = env
        .bougie()
        .args(["composer", "show", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "should fail without a lock");
    let s = String::from_utf8_lossy(&out.stderr);
    assert!(s.contains("composer.lock"), "{s}");
}
