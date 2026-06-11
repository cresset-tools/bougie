//! Integration tests for `bougie composer licenses` / `fund` /
//! `status` — all offline, reading a staged lock.

use std::path::Path;
use tempfile::TempDir;

mod common;
use common::TestEnv;

fn stage(dir: &Path, composer_json: &str, lock: &str) {
    std::fs::write(dir.join("composer.json"), composer_json).unwrap();
    std::fs::write(dir.join("composer.lock"), lock).unwrap();
}

const COMPOSER_JSON: &str = r#"{"name":"test/proj","license":"MIT","require":{"acme/lib":"^2.0"}}"#;
const LOCK: &str = r#"{"content-hash":"x","packages":[
    {"name":"acme/lib","version":"2.3.0","license":["MIT"],
     "funding":[{"type":"github","url":"https://github.com/sponsors/acme"}]},
    {"name":"psr/log","version":"3.0.0","license":["MIT","Apache-2.0"]},
    {"name":"acme/nolicense","version":"1.0.0"}
],"packages-dev":[
    {"name":"phpunit/phpunit","version":"10.5.0","license":["BSD-3-Clause"]}
]}"#;

#[test]
fn licenses_lists_each_package() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    let out = env.bougie().args(["composer", "licenses", "-d"]).arg(proj.path()).output().unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("test/proj"), "{s}");
    assert!(s.contains("acme/lib") && s.contains("MIT"), "{s}");
    assert!(s.contains("Apache-2.0"), "multi-license joined: {s}");
    // Undeclared license shows as "none".
    assert!(s.contains("none"), "{s}");
    assert!(s.contains("Summary"), "{s}");
}

#[test]
fn licenses_no_dev_excludes_dev() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    let out = env
        .bougie()
        .args(["composer", "licenses", "--no-dev", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(!s.contains("phpunit/phpunit"), "dev pkg hidden: {s}");
}

#[test]
fn licenses_json_format() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    let out = env
        .bougie()
        .args(["composer", "licenses", "--format", "json", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(v.get("project").and_then(|p| p.as_str()), Some("test/proj"));
    assert!(v.get("dependencies").and_then(|d| d.as_array()).unwrap().len() >= 3);
}

#[test]
fn fund_groups_by_vendor() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    let out = env.bougie().args(["composer", "fund", "-d"]).arg(proj.path()).output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("acme"), "vendor group: {s}");
    assert!(s.contains("acme/lib"), "{s}");
    assert!(s.contains("github.com/sponsors/acme"), "{s}");
    // psr/log has no funding → not listed.
    assert!(!s.contains("psr/log"), "{s}");
}

#[test]
fn fund_empty_when_no_funding() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    let lock = r#"{"content-hash":"x","packages":[{"name":"psr/log","version":"3.0.0"}],"packages-dev":[]}"#;
    stage(proj.path(), COMPOSER_JSON, lock);

    let out = env.bougie().args(["composer", "fund", "-d"]).arg(proj.path()).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("No funding links"), "{s}");
}

#[test]
fn status_reports_no_local_changes_for_dist_installs() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), COMPOSER_JSON, LOCK);

    let out = env.bougie().args(["composer", "status", "-d"]).arg(proj.path()).output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("No local changes"), "{s}");
}

#[test]
fn status_lists_path_packages() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    let lock = r#"{"content-hash":"x","packages":[
        {"name":"acme/local","version":"1.0.0",
         "dist":{"type":"path","url":"../local"}}
    ],"packages-dev":[]}"#;
    stage(proj.path(), COMPOSER_JSON, lock);

    let out = env.bougie().args(["composer", "status", "-d"]).arg(proj.path()).output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("acme/local"), "path-dist package should be listed: {s}");
    assert!(s.contains("local path"), "{s}");
}
