//! Phase 9: end-to-end `bougie composer …` against a wiremock stand-in
//! for getcomposer.org. Exercises install / list / find / pin and the
//! sync integration.

use predicates::str::contains;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

mod common;
use common::TestEnv;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

struct Fixture {
    server: MockServer,
    phar_bytes: Vec<u8>,
    phar_sha: String,
}

async fn build_fixture() -> Fixture {
    let server = MockServer::start().await;
    let phar_bytes = b"#!/usr/bin/env php\n<?php echo 'fake composer';\n".to_vec();
    let phar_sha = hex(&phar_bytes);

    let channels = serde_json::json!({
        "stable": [
            {"version": "2.8.5", "path": "/download/2.8.5/composer.phar", "shasum": phar_sha},
            {"version": "2.8.4", "path": "/download/2.8.4/composer.phar", "shasum": "deadbeef"},
            {"version": "2.7.9", "path": "/download/2.7.9/composer.phar", "shasum": "deadbeef"},
        ],
        "preview": []
    });
    let channels_bytes = serde_json::to_vec(&channels).unwrap();

    Mock::given(method("GET")).and(path("/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(channels_bytes).insert_header("etag", "\"v1\""))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/download/2.8.5/composer.phar"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(phar_bytes.clone()))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/download/2.8.5/composer.phar.sha256sum"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{phar_sha}  composer.phar")))
        .mount(&server).await;

    Fixture { server, phar_bytes, phar_sha }
}

#[test]
fn install_fetches_phar_and_writes_to_bougie_home_composer() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();

    env.bougie()
        .env("BOUGIE_COMPOSER_BASE_URL", fx.server.uri())
        .args(["composer", "fetch", "2.8.5"])
        .assert()
        .success()
        .stdout(contains("installed composer 2.8.5"));

    let phar = env.home_path().join("composer/2.8.5/composer.phar");
    assert!(phar.is_file(), "phar should exist at {}", phar.display());
    assert_eq!(std::fs::read(&phar).unwrap(), fx.phar_bytes);
}

#[test]
fn install_resolves_partial_version() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();

    env.bougie()
        .env("BOUGIE_COMPOSER_BASE_URL", fx.server.uri())
        .args(["composer", "fetch", "2.8"])
        .assert()
        .success()
        .stdout(contains("installed composer 2.8.5"));
    let _ = fx.phar_sha;
}

#[test]
fn install_default_picks_stable_head() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();

    env.bougie()
        .env("BOUGIE_COMPOSER_BASE_URL", fx.server.uri())
        .args(["composer", "fetch"])
        .assert()
        .success()
        .stdout(contains("installed composer 2.8.5"));
    let _ = fx.phar_sha;
}

#[test]
fn install_is_idempotent() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();

    env.bougie()
        .env("BOUGIE_COMPOSER_BASE_URL", fx.server.uri())
        .args(["composer", "fetch", "2.8.5"])
        .assert()
        .success();

    env.bougie()
        .env("BOUGIE_COMPOSER_BASE_URL", fx.server.uri())
        .args(["composer", "fetch", "2.8.5"])
        .assert()
        .success()
        .stdout(contains("already composer 2.8.5"));
}

#[test]
fn install_rejects_phar_bytes_disagreeing_with_sha256sum_endpoint() {
    // The .sha256sum endpoint is the single trust anchor: if the
    // delivered phar doesn't hash to the value advertised there,
    // bougie must refuse to install.
    let runtime = rt();
    let server = runtime.block_on(MockServer::start());
    let phar_bytes = b"hello world".to_vec();
    let lying_sha = "0".repeat(64); // not the sha of phar_bytes
    runtime.block_on(async {
        let channels = serde_json::json!({
            "stable": [{"version":"2.8.5","path":"/download/2.8.5/composer.phar"}],
            "preview": []
        });
        Mock::given(method("GET")).and(path("/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(serde_json::to_vec(&channels).unwrap()))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/download/2.8.5/composer.phar"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(phar_bytes.clone()))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/download/2.8.5/composer.phar.sha256sum"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{lying_sha}  composer.phar")))
            .mount(&server).await;
    });

    let env = TestEnv::new();
    env.bougie()
        .env("BOUGIE_COMPOSER_BASE_URL", server.uri())
        .args(["composer", "fetch", "2.8.5"])
        .assert()
        .failure()
        .stderr(contains("sha256 mismatch"));
}

#[test]
fn dir_prints_composer_root() {
    let env = TestEnv::new();
    let out = env
        .bougie()
        .args(["composer", "dir"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let line = String::from_utf8(out).unwrap();
    assert!(line.trim().ends_with("/composer"), "got: {line}");
}

#[test]
fn list_with_nothing_installed_says_so() {
    let env = TestEnv::new();
    env.bougie()
        .args(["composer", "list"])
        .assert()
        .success()
        .stdout(contains("no composer versions installed"));
}

#[test]
fn find_after_install_returns_phar_path() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();

    env.bougie()
        .env("BOUGIE_COMPOSER_BASE_URL", fx.server.uri())
        .args(["composer", "fetch", "2.8.5"])
        .assert()
        .success();

    let out = env
        .bougie()
        .args(["composer", "find", "2.8.5"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let line = String::from_utf8(out).unwrap();
    assert!(line.trim().ends_with("/composer/2.8.5/composer.phar"), "got: {line}");
}

#[test]
fn find_without_arg_picks_highest_installed() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();

    env.bougie()
        .env("BOUGIE_COMPOSER_BASE_URL", fx.server.uri())
        .args(["composer", "fetch", "2.8.5"])
        .assert()
        .success();

    let out = env
        .bougie()
        .args(["composer", "find"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8(out).unwrap().contains("/composer/2.8.5/composer.phar"));
}

#[test]
fn find_with_no_install_errors() {
    let env = TestEnv::new();
    env.bougie()
        .args(["composer", "find"])
        .assert()
        .failure()
        .stderr(contains("no composer installed"));
}

#[test]
fn pin_writes_to_bougie_toml_when_present() {
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(proj.path().join("bougie.toml"), "").unwrap();

    env.bougie()
        .current_dir(proj.path())
        .args(["composer", "pin", "2.8.5"])
        .assert()
        .success()
        .stdout(contains("bougie.toml"));

    let body = std::fs::read_to_string(proj.path().join("bougie.toml")).unwrap();
    assert!(body.contains("[composer]"), "body: {body}");
    assert!(body.contains(r#"version = "2.8.5""#), "body: {body}");
}

#[test]
fn pin_falls_back_to_composer_json_when_no_toml() {
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(proj.path().join("composer.json"), "{}\n").unwrap();

    env.bougie()
        .current_dir(proj.path())
        .args(["composer", "pin", "2.8.5"])
        .assert()
        .success()
        .stdout(contains("composer.json"));

    let body = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["extra"]["bougie"]["composer"]["version"].as_str(),
        Some("2.8.5")
    );
}

/// Counts requests so we can assert idempotency doesn't re-fetch.
#[derive(Clone)]
struct CountingResponder {
    template: Arc<ResponseTemplate>,
    counter: Arc<AtomicUsize>,
}
impl Respond for CountingResponder {
    fn respond(&self, _req: &wiremock::Request) -> ResponseTemplate {
        self.counter.fetch_add(1, Ordering::SeqCst);
        (*self.template).clone()
    }
}

#[test]
fn idempotent_install_does_not_refetch_phar() {
    let runtime = rt();
    let server = runtime.block_on(MockServer::start());
    let phar_bytes = b"#!/usr/bin/env php\n<?php echo 1;\n".to_vec();
    let phar_sha = hex(&phar_bytes);
    let phar_counter = Arc::new(AtomicUsize::new(0));

    runtime.block_on(async {
        let channels = serde_json::json!({
            "stable": [{"version":"2.8.5","path":"/download/2.8.5/composer.phar","shasum": phar_sha}],
            "preview": []
        });
        Mock::given(method("GET")).and(path("/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(serde_json::to_vec(&channels).unwrap()))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/download/2.8.5/composer.phar"))
            .respond_with(CountingResponder {
                template: Arc::new(ResponseTemplate::new(200).set_body_bytes(phar_bytes.clone())),
                counter: phar_counter.clone(),
            })
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/download/2.8.5/composer.phar.sha256sum"))
            .respond_with(ResponseTemplate::new(200).set_body_string(phar_sha.clone()))
            .mount(&server).await;
    });

    let env = TestEnv::new();
    for _ in 0..2 {
        env.bougie()
            .env("BOUGIE_COMPOSER_BASE_URL", server.uri())
            .args(["composer", "fetch", "2.8.5"])
            .assert()
            .success();
    }
    assert_eq!(
        phar_counter.load(Ordering::SeqCst),
        1,
        "phar should only be fetched once across two installs"
    );
    let _ = phar_sha;
}
