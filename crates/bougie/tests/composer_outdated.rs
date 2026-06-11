//! Integration tests for `bougie composer outdated`. A wiremock server
//! stands in for Packagist; `BOUGIE_PACKAGIST_BASE_URL` points the
//! `latest_versions` lookup at it.

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

fn stage(dir: &Path, lock: &str) {
    std::fs::write(dir.join("composer.json"), r#"{"name":"test/p","require":{"acme/lib":"^2.0"}}"#)
        .unwrap();
    std::fs::write(dir.join("composer.lock"), lock).unwrap();
}

const LOCK_LIB_200: &str = r#"{"content-hash":"x","packages":[
    {"name":"acme/lib","version":"2.0.0","version_normalized":"2.0.0.0"}
],"packages-dev":[]}"#;

fn serve(foo_versions: &[&str]) -> (String, MockServer) {
    let body = p2_body("acme/lib", foo_versions);
    let rt = rt();
    rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/lib.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    })
}

#[test]
fn outdated_reports_newer_version() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), LOCK_LIB_200);
    // A newer 2.5.0 is available (minor bump).
    let (uri, _server) = serve(&["2.5.0", "2.0.0"]);

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "outdated", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("acme/lib"), "{s}");
    assert!(s.contains("2.0.0"), "{s}");
    assert!(s.contains("2.5.0"), "latest shown: {s}");
}

#[test]
fn outdated_up_to_date_is_quiet() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), LOCK_LIB_200);
    // Only 2.0.0 available — nothing newer.
    let (uri, _server) = serve(&["2.0.0"]);

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "outdated", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("up to date"), "{s}");
}

#[test]
fn outdated_major_only_filters_minor_bumps() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), LOCK_LIB_200);
    // 2.5.0 is a minor bump — --major-only should hide it.
    let (uri, _server) = serve(&["2.5.0", "2.0.0"]);

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "outdated", "--major-only", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("up to date"), "minor bump hidden by --major-only: {s}");
}

#[test]
fn outdated_strict_exits_non_zero() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), LOCK_LIB_200);
    let (uri, _server) = serve(&["2.5.0", "2.0.0"]);

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "outdated", "--strict", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "--strict must fail when outdated packages exist");
}

#[test]
fn outdated_json_format() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), LOCK_LIB_200);
    let (uri, _server) = serve(&["3.0.0", "2.0.0"]);

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "outdated", "--format", "json", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let rows = v.get("rows").and_then(|r| r.as_array()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("bump").and_then(|b| b.as_str()), Some("major"));
}
