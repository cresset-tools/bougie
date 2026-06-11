//! Integration tests for `bougie composer audit`. A wiremock server
//! stands in for the Packagist security-advisories API;
//! `BOUGIE_AUDIT_BASE_URL` points the client at it.

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

fn stage(dir: &Path, lock: &str) {
    std::fs::write(dir.join("composer.json"), r#"{"name":"test/p","require":{"acme/lib":"^2.0"}}"#)
        .unwrap();
    std::fs::write(dir.join("composer.lock"), lock).unwrap();
}

const LOCK: &str = r#"{"content-hash":"x","packages":[
    {"name":"acme/lib","version":"2.0.0","version_normalized":"2.0.0.0"}
],"packages-dev":[]}"#;

/// Mount the advisories endpoint with a canned response body.
fn serve(body: &'static str) -> (String, MockServer) {
    let rt = rt();
    rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/security-advisories/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    })
}

#[test]
fn audit_reports_matching_advisory_and_fails() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), LOCK);

    // Advisory affects >=1.0,<2.1 — the locked 2.0.0 is in range.
    let (uri, _server) = serve(
        r#"{"advisories":{"acme/lib":[
            {"advisoryId":"PKSA-1","packageName":"acme/lib",
             "affectedVersions":">=1.0,<2.1","title":"RCE in acme/lib",
             "cve":"CVE-2026-0001","severity":"high",
             "link":"https://example/advisory/1"}
        ]}}"#,
    );

    let out = env
        .bougie()
        .env("BOUGIE_AUDIT_BASE_URL", &uri)
        .args(["composer", "audit", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    // Findings → non-zero exit.
    assert!(!out.status.success(), "audit must fail when advisories match");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("acme/lib"), "{s}");
    assert!(s.contains("PKSA-1"), "{s}");
    assert!(s.contains("CVE-2026-0001"), "{s}");
}

#[test]
fn audit_ignores_advisory_outside_locked_range() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), LOCK);

    // Advisory only affects <2.0 — the locked 2.0.0 is NOT vulnerable.
    let (uri, _server) = serve(
        r#"{"advisories":{"acme/lib":[
            {"advisoryId":"PKSA-2","packageName":"acme/lib",
             "affectedVersions":"<2.0","title":"old bug"}
        ]}}"#,
    );

    let out = env
        .bougie()
        .env("BOUGIE_AUDIT_BASE_URL", &uri)
        .args(["composer", "audit", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "no in-range advisory → success: stderr={}",
        String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("No security vulnerability"), "{s}");
}

#[test]
fn audit_clean_when_no_advisories() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), LOCK);
    let (uri, _server) = serve(r#"{"advisories":{}}"#);

    let out = env
        .bougie()
        .env("BOUGIE_AUDIT_BASE_URL", &uri)
        .args(["composer", "audit", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("No security vulnerability"), "{s}");
    assert!(s.contains("1 packages audited"), "{s}");
}

#[test]
fn audit_json_format() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    stage(proj.path(), LOCK);
    let (uri, _server) = serve(
        r#"{"advisories":{"acme/lib":[
            {"advisoryId":"PKSA-1","packageName":"acme/lib",
             "affectedVersions":">=1.0,<2.1","title":"RCE"}
        ]}}"#,
    );

    let out = env
        .bougie()
        .env("BOUGIE_AUDIT_BASE_URL", &uri)
        .args(["composer", "audit", "--format", "json", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    let findings = v.get("findings").and_then(|f| f.as_array()).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(
        findings[0].get("advisory_id").and_then(|a| a.as_str()),
        Some("PKSA-1")
    );
}
