//! Integration tests for `bougie composer install` (project install).
//!
//! Drives the real `bougie` binary via `assert_cmd`, with a wiremock
//! server standing in for Packagist. Each test stages a tiny project
//! (composer.json + composer.lock) pointing at the mock URL, runs
//! `bougie composer install -d <project>`, and asserts on the
//! resulting `vendor/` tree.

use assert_cmd::Command;
use std::io::Write;
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

fn sha1_hex(bytes: &[u8]) -> String {
    use sha1::Digest as _;
    let digest = sha1::Sha1::digest(bytes);
    let mut s = String::with_capacity(40);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Build a fixture Composer dist zip wrapping entries in
/// `<top>/...`, with a single PSR-4 source file the autoloader will
/// pick up.
fn build_fixture_zip(top: &str) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file(format!("{top}/composer.json"), opts).unwrap();
        zw.write_all(br#"{"name":"acme/foo","autoload":{"psr-4":{"Acme\\Foo\\":"src/"}}}"#)
            .unwrap();
        zw.start_file(format!("{top}/src/Foo.php"), opts).unwrap();
        zw.write_all(b"<?php\nnamespace Acme\\Foo; class Foo {}\n")
            .unwrap();
        zw.finish().unwrap();
    }
    buf
}

fn write_project_files(dir: &Path, composer_json: &str, composer_lock: &str) {
    std::fs::write(dir.join("composer.json"), composer_json).unwrap();
    std::fs::write(dir.join("composer.lock"), composer_lock).unwrap();
}

#[test]
fn install_against_wiremock_dist_emits_vendor_and_autoload() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    // Fixture dist + valid sha1.
    let top = "acme-foo-aaaaaaaaaa";
    let body = build_fixture_zip(top);
    let hash = sha1_hex(&body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    // The composer.json's content-hash is computed by bougie's own
    // hasher; let the install verify it. We use a fixed minimal
    // composer.json + look up its actual hash at test time so the
    // lock embeds the right value.
    let composer_json = r#"{
    "name": "test/project",
    "require": {"acme/foo": "^1.0"}
}"#;
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
        "content-hash": "{content_hash}",
        "packages": [
            {{
                "name": "acme/foo",
                "version": "1.0.0",
                "dist": {{
                    "type": "zip",
                    "url": "{uri}/dists/acme-foo.zip",
                    "shasum": "{hash}"
                }},
                "type": "library",
                "autoload": {{"psr-4": {{"Acme\\Foo\\": "src/"}}}}
            }}
        ],
        "packages-dev": []
    }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    let mut cmd = env.bougie();
    cmd.args(["composer", "install", "-d"])
        .arg(proj.path())
        .assert()
        .success();

    // The downloader extracted the zip into vendor/acme/foo, stripped
    // the wrapping `acme-foo-aaaaaaaaaa/` dir.
    let vendor_foo = proj.path().join("vendor").join("acme").join("foo");
    assert!(vendor_foo.is_dir());
    assert!(vendor_foo.join("composer.json").is_file());
    assert!(vendor_foo.join("src/Foo.php").is_file());
    assert!(!vendor_foo.join(top).exists());

    // bougie-autoloader emitted the standard surface.
    assert!(proj.path().join("vendor/autoload.php").is_file());
    assert!(proj.path().join("vendor/composer/autoload_psr4.php").is_file());
    assert!(proj.path().join("vendor/composer/installed.json").is_file());
    assert!(proj.path().join("vendor/composer/installed.php").is_file());
}

#[test]
fn install_fails_with_helpful_message_when_lock_missing() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"name":"test/lonely","require":{}}"#,
    )
    .unwrap();

    let output = env
        .bougie()
        .args(["composer", "install", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");
    assert!(!output.status.success(), "expected failure exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("composer.lock"), "{stderr}");
    assert!(stderr.contains("composer update"), "{stderr}");
}

#[test]
fn install_fails_when_lock_declares_composer_plugin() {
    // End-to-end check that the preflight error reaches the CLI exit
    // path (not just the orchestrator unit test).
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    let composer_json = r#"{"name":"test/plug","require":{}}"#;
    let hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [
                {{
                    "name": "evil/plugin",
                    "version": "1.0.0",
                    "type": "composer-plugin",
                    "dist": {{
                        "type": "zip",
                        "url": "https://example/p.zip",
                        "shasum": "1111111111111111111111111111111111111111"
                    }}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    let output = env
        .bougie()
        .args(["composer", "install", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("evil/plugin"), "{stderr}");
    assert!(stderr.contains("bougie run -- composer install"), "{stderr}");
}

#[test]
fn lock_verify_returns_zero_on_valid_lock() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    let composer_json = r#"{"name":"test/ok","require":{"acme/foo":"^1.2"}}"#;
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
            "content-hash": "{content_hash}",
            "packages": [
                {{
                    "name": "acme/foo",
                    "version": "1.2.3",
                    "dist": {{"type":"zip","url":"https://e/f.zip","shasum":"aa"}}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    let output = env
        .bougie()
        .args(["composer", "install", "--lock-verify", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("valid"), "{stdout}");
    // No vendor/ should be created for the verify path.
    assert!(!proj.path().join("vendor").exists());
}

#[test]
fn lock_verify_returns_non_zero_on_invalid_lock() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    // Root says ^2 but lock pins 1.5 — invalid.
    let composer_json = r#"{"name":"test/bad","require":{"acme/foo":"^2"}}"#;
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
            "content-hash": "{content_hash}",
            "packages": [
                {{
                    "name": "acme/foo",
                    "version": "1.5.0",
                    "dist": {{"type":"zip","url":"https://e/f.zip","shasum":"aa"}}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project_files(proj.path(), composer_json, &lock);

    let output = env
        .bougie()
        .args(["composer", "install", "--lock-verify", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");
    assert!(!output.status.success(), "expected non-zero exit");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("INVALID"), "{stdout}");
    assert!(stdout.contains("acme/foo"), "must name the conflicting pkg: {stdout}");
}

/// Use the binary `Command` API directly here so `cargo build -p
/// bougie --tests` still exercises this file even with --quiet.
#[allow(dead_code)]
fn _ensure_command_imported(_: Command) {}
