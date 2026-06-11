//! Integration tests for `bougie composer require` / `remove`.
//!
//! Stages a tiny project, points `BOUGIE_PACKAGIST_BASE_URL` at a
//! wiremock server, and exercises the native (no-phar) require/remove
//! flow. `--no-install` keeps the tests off the dist downloader (the
//! mock serves metadata, not real zip archives); `--no-update` keeps
//! the explicit-constraint path fully offline.

use std::io::Write;
use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::TestEnv;

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

/// Build a minimal Composer dist zip wrapping a single PSR-4 source
/// file under `<top>/`, so the autoloader has something to pick up.
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

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn write_composer_json(project_dir: &Path, body: &str) {
    std::fs::write(project_dir.join("composer.json"), body).unwrap();
}

fn p2_body(name: &str, versions: &[(&str, &str)]) -> String {
    let entries: Vec<String> = versions
        .iter()
        .map(|(v, req)| {
            format!(
                r#"{{
                    "name":"{name}","version":"{v}","version_normalized":"{v}.0",
                    "type":"library",
                    "dist":{{"type":"zip","url":"https://e/{name}/{v}.zip","shasum":"aa"}},
                    "require":{req}
                }}"#
            )
        })
        .collect();
    format!(r#"{{"packages":{{"{name}":[{}]}}}}"#, entries.join(","))
}

#[test]
fn require_bare_name_writes_caret_of_latest_stable() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let foo = p2_body("acme/foo", &[("2.0.0", "{}"), ("1.5.0", "{}"), ("1.0.0", "{}")]);

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

    let output = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "require", "acme/foo", "--no-install", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");

    assert!(
        output.status.success(),
        "exit failed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    // composer.json now carries the caret constraint of the latest
    // stable (2.0.0 → ^2.0).
    let cj = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(cj.contains("\"acme/foo\""), "composer.json: {cj}");
    assert!(cj.contains("^2.0"), "expected caret of latest stable: {cj}");

    // The lock was written; vendor/ was not (--no-install).
    assert!(proj.path().join("composer.lock").is_file());
    assert!(!proj.path().join("vendor").exists());
}

#[test]
fn require_explicit_constraint_no_update_is_offline_json_edit() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    write_composer_json(proj.path(), r#"{"name":"test/p","require":{}}"#);

    // No mock server at all — an explicit constraint with --no-update
    // touches only composer.json, never the network.
    let output = env
        .bougie()
        .args([
            "composer",
            "require",
            "acme/foo:^1.2",
            "--no-update",
            "-d",
        ])
        .arg(proj.path())
        .output()
        .expect("run bougie");

    assert!(
        output.status.success(),
        "exit failed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    let cj = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(cj.contains("\"acme/foo\""), "composer.json: {cj}");
    assert!(cj.contains("^1.2"), "explicit constraint stored verbatim: {cj}");
    assert!(!proj.path().join("composer.lock").exists(), "--no-update: no lock");
    assert!(!proj.path().join("vendor").exists());
}

#[test]
fn require_dev_targets_require_dev() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    write_composer_json(proj.path(), r#"{"name":"test/p","require":{}}"#);

    let output = env
        .bougie()
        .args([
            "composer",
            "require",
            "phpunit/phpunit:^10.5",
            "--dev",
            "--no-update",
            "-d",
        ])
        .arg(proj.path())
        .output()
        .expect("run bougie");

    assert!(output.status.success(), "stderr={}", String::from_utf8_lossy(&output.stderr));

    let cj = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(cj.contains("require-dev"), "should add a require-dev block: {cj}");
    assert!(cj.contains("phpunit/phpunit"), "{cj}");
}

#[test]
fn require_at_separator_is_invalid_like_composer() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    write_composer_json(proj.path(), r#"{"name":"test/p","require":{}}"#);

    // `@` is not a Composer separator: the whole token becomes the
    // package name, which can't be found → non-zero exit. (No mock, so
    // even a real lookup would fail; the point is bougie doesn't
    // "helpfully" reinterpret `@` as a constraint separator.)
    let output = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", "http://127.0.0.1:1/none")
        .args(["composer", "require", "acme/foo@^1.0", "--no-install", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");

    assert!(
        !output.status.success(),
        "`vendor/pkg@constraint` must not succeed (it's invalid in Composer)",
    );
}

#[test]
fn require_with_install_materializes_vendor() {
    // Full flow with NO --no-install: resolve latest stable → caret →
    // edit composer.json → write composer.lock → download the dist →
    // materialize vendor/ + emit the autoloader. The metadata's dist
    // URL and the dist itself are both served by the same mock.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let top = "acme-foo-aaaaaaaaaa";
    let zip_body = build_fixture_zip(top);
    let dist_hash = sha1_hex(&zip_body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        let server_uri = server.uri();
        let metadata = format!(
            r#"{{"packages":{{"acme/foo":[{{
                "name":"acme/foo",
                "version":"1.0.0",
                "version_normalized":"1.0.0.0",
                "type":"library",
                "dist":{{"type":"zip","url":"{server_uri}/dists/acme-foo.zip","shasum":"{dist_hash}"}},
                "autoload":{{"psr-4":{{"Acme\\Foo\\":"src/"}}}}
            }}]}}}}"#,
        );
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/foo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(metadata))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_body))
            .mount(&server)
            .await;
        (server_uri, server)
    });

    write_composer_json(proj.path(), r#"{"name":"test/p","require":{}}"#);

    let output = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "require", "acme/foo", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");

    assert!(
        output.status.success(),
        "exit failed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    // composer.json caret, lock written.
    let cj = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(cj.contains("^1.0"), "composer.json: {cj}");
    assert!(proj.path().join("composer.lock").is_file());

    // The dist was actually downloaded, extracted (wrapping dir
    // stripped), and the autoloader emitted.
    let vendor_foo = proj.path().join("vendor").join("acme").join("foo");
    assert!(vendor_foo.join("src/Foo.php").is_file(), "vendor not materialized");
    assert!(!vendor_foo.join(top).exists(), "wrapping dir should be stripped");
    assert!(proj.path().join("vendor/autoload.php").is_file());
    assert!(proj.path().join("vendor/composer/installed.json").is_file());
}

#[test]
fn require_partial_relock_keeps_other_packages_pinned() {
    // The partial-relock branch: with an existing lock pinning acme/foo
    // at 1.0.0, requiring a NEW package acme/bar must add bar WITHOUT
    // bumping foo to the newer 1.5.0 that the repo also offers. That's
    // Composer's minimal-change `require` behavior, driven by
    // PartialUpdate (names scoped to the newly-required packages).
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let foo = p2_body("acme/foo", &[("1.5.0", "{}"), ("1.0.0", "{}")]);
    let bar = p2_body("acme/bar", &[("1.0.0", "{}")]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/foo.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(foo))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/bar.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(bar))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    // Existing project: foo required at ^1.0 and locked at 1.0.0.
    let composer_json = r#"{"name":"test/p","require":{"acme/foo":"^1.0"}}"#;
    write_composer_json(proj.path(), composer_json);
    let content_hash =
        bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{
        "content-hash": "{content_hash}",
        "packages": [
            {{"name":"acme/foo","version":"1.0.0","version_normalized":"1.0.0.0",
              "dist":{{"type":"zip","url":"https://e/acme/foo/1.0.0.zip","shasum":"aa"}}}}
        ],
        "packages-dev": []
    }}"#
    );
    std::fs::write(proj.path().join("composer.lock"), lock).unwrap();

    // Require a NEW package; --no-install keeps us off the dist
    // downloader (this test is about the lock's resolution outcome).
    let output = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "require", "acme/bar", "--no-install", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");

    assert!(
        output.status.success(),
        "exit failed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    let lock_after =
        std::fs::read_to_string(proj.path().join("composer.lock")).unwrap();
    assert!(lock_after.contains("acme/bar"), "bar should be added: {lock_after}");
    assert!(lock_after.contains("acme/foo"), "foo should remain: {lock_after}");
    // The crux: foo stays pinned at 1.0.0 (partial update), NOT bumped
    // to the available 1.5.0.
    assert!(
        lock_after.contains("\"1.0.0\""),
        "foo must stay pinned at 1.0.0: {lock_after}",
    );
    assert!(
        !lock_after.contains("\"1.5.0\""),
        "foo must NOT be bumped to 1.5.0 by a partial require: {lock_after}",
    );
}

#[test]
fn remove_no_update_drops_the_require_entry() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    write_composer_json(
        proj.path(),
        r#"{"name":"test/p","require":{"acme/foo":"^1.0","acme/bar":"^2.0"}}"#,
    );

    let output = env
        .bougie()
        .args(["composer", "remove", "acme/foo", "--no-update", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");

    assert!(output.status.success(), "stderr={}", String::from_utf8_lossy(&output.stderr));

    let cj = std::fs::read_to_string(proj.path().join("composer.json")).unwrap();
    assert!(!cj.contains("acme/foo"), "acme/foo should be gone: {cj}");
    assert!(cj.contains("acme/bar"), "acme/bar should remain: {cj}");
}
