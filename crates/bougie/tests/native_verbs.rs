//! Integration tests for the uv-style top-level verbs `bougie add` /
//! `bougie remove`. They share the engine behind `composer require` /
//! `remove` but differ in supply syntax (`@`) and default constraint
//! (`>=` lower bound vs caret). `--frozen` keeps the explicit-constraint
//! path fully offline; `--no-sync` keeps the bare-name path off the dist
//! downloader (the mock serves metadata, not zip archives).

use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::TestEnv;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn write_composer_json(dir: &Path, body: &str) {
    std::fs::write(dir.join("composer.json"), body).unwrap();
}

fn p2_body(name: &str, versions: &[&str]) -> String {
    let entries: Vec<String> = versions
        .iter()
        .map(|v| {
            format!(
                r#"{{"name":"{name}","version":"{v}","version_normalized":"{v}.0","type":"library",
                    "dist":{{"type":"zip","url":"https://e/{name}/{v}.zip","shasum":"aa"}}}}"#
            )
        })
        .collect();
    format!(r#"{{"packages":{{"{name}":[{}]}}}}"#, entries.join(","))
}

#[test]
fn add_bare_name_writes_lower_bound_of_latest_stable() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let foo = p2_body("acme/foo", &["2.3.0", "2.0.0", "1.0.0"]);
    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/foo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(foo))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    write_composer_json(proj.path(), r#"{"name":"test/p","require":{}}"#);

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["add", "acme/foo", "--no-sync", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));

    let cj = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    // uv-style lower bound of the latest stable (2.3.0 → >=2.3), NOT a caret.
    assert!(cj.contains("\"acme/foo\""), "{cj}");
    assert!(cj.contains(">=2.3"), "expected lower-bound default: {cj}");
    assert!(!cj.contains("^2.3"), "must not be a caret: {cj}");
    // lock written, vendor/ not (--no-sync).
    assert!(proj.path().join("composer.lock").is_file());
    assert!(!proj.path().join("vendor").exists());
}

#[test]
fn add_explicit_at_constraint_frozen_is_offline() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    write_composer_json(proj.path(), r#"{"name":"test/p","require":{}}"#);

    // No mock server: `@`-explicit constraint + --frozen touches only
    // composer.json.
    let out = env
        .bougie()
        .args(["add", "acme/foo@^1.2", "--frozen", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));

    let cj = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(cj.contains("^1.2"), "explicit @ constraint stored verbatim: {cj}");
    assert!(!proj.path().join("composer.lock").exists(), "--frozen: no lock");
}

#[test]
fn add_dev_targets_require_dev() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    write_composer_json(proj.path(), r#"{"name":"test/p","require":{}}"#);

    let out = env
        .bougie()
        .args(["add", "phpunit/phpunit@^10.5", "--dev", "--frozen", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let cj = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(cj.contains("require-dev"), "{cj}");
    assert!(cj.contains("phpunit/phpunit"), "{cj}");
}

#[test]
fn add_empty_version_after_at_errors() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    write_composer_json(proj.path(), r#"{"name":"test/p","require":{}}"#);

    let out = env
        .bougie()
        .args(["add", "acme/foo@", "--frozen", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "empty version after `@` must error");
}

#[test]
fn tree_renders_project_hierarchy() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    write_composer_json(proj.path(), r#"{"name":"test/p","require":{"acme/lib":"^2.0"}}"#);
    std::fs::write(
        proj.path().join("composer.lock"),
        r#"{"content-hash":"x","packages":[
            {"name":"acme/lib","version":"2.3.0","require":{"psr/log":"^3.0"}},
            {"name":"psr/log","version":"3.0.0"}
        ],"packages-dev":[]}"#,
    )
    .unwrap();

    let out = env.bougie().args(["tree", "-d"]).arg(proj.path()).output().unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("test/p"), "{s}");
    assert!(s.contains("acme/lib ^2.0"), "{s}");
    assert!(s.contains("psr/log ^3.0"), "nested transitive: {s}");
}

#[test]
fn outdated_reports_newer_version() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    write_composer_json(proj.path(), r#"{"name":"test/p","require":{"acme/foo":"^2.0"}}"#);
    std::fs::write(
        proj.path().join("composer.lock"),
        r#"{"content-hash":"x","packages":[
            {"name":"acme/foo","version":"2.0.0","version_normalized":"2.0.0.0"}
        ],"packages-dev":[]}"#,
    )
    .unwrap();

    let foo = p2_body("acme/foo", &["2.5.0", "2.0.0"]);
    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/foo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(foo))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["outdated", "--strict", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    // --strict → non-zero when something is outdated.
    assert!(!out.status.success(), "strict outdated should exit non-zero");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("acme/foo"), "{s}");
    assert!(s.contains("2.5.0"), "latest shown: {s}");
}

#[test]
fn remove_frozen_drops_entry() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    write_composer_json(
        proj.path(),
        r#"{"name":"test/p","require":{"acme/foo":">=1.0","acme/bar":">=2.0"}}"#,
    );

    let out = env
        .bougie()
        .args(["remove", "acme/foo", "--frozen", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let cj = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(!cj.contains("acme/foo"), "{cj}");
    assert!(cj.contains("acme/bar"), "{cj}");
}
