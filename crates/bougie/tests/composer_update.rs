//! Integration tests for `bougie composer update --dry-run`.
//!
//! Each test stages a tiny project (composer.json only — the update
//! verb deliberately ignores any existing lock), spins up a wiremock
//! server with one or more `/p2/` responses, and runs the binary via
//! `assert_cmd` with `BOUGIE_PACKAGIST_BASE_URL` pointing at the
//! mock.

use assert_cmd::Command;
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

fn write_composer_json(project_dir: &Path, body: &str) {
    std::fs::write(project_dir.join("composer.json"), body).unwrap();
}

fn p2_body(name: &str, versions: &[(&str, &str)]) -> String {
    // `versions` is a slice of (version, require_json). `require_json`
    // is embedded raw so callers can write `{}` for "no requires" or
    // a full object inline.
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
fn dry_run_resolves_against_wiremock_and_prints_packages() {
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

    write_composer_json(
        proj.path(),
        r#"{"name":"test/p","require":{"acme/foo":"^1.0"}}"#,
    );

    let mut cmd = env.bougie();
    let output = cmd
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "update", "--dry-run", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");

    assert!(
        output.status.success(),
        "exit failed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("acme/foo"), "{stdout}");
    assert!(stdout.contains("1.5.0"), "must pick highest in range: {stdout}");
    // No vendor/ should appear — dry-run is read-only.
    assert!(!proj.path().join("vendor").exists());
}

#[test]
fn dry_run_resolves_transitive_dependency() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let foo = p2_body("acme/foo", &[("1.0.0", r#"{"acme/bar":"^2.0"}"#)]);
    let bar = p2_body("acme/bar", &[("2.3.0", "{}"), ("2.0.0", "{}")]);

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

    write_composer_json(
        proj.path(),
        r#"{"name":"test/p","require":{"acme/foo":"^1.0"}}"#,
    );

    let output = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "update", "--dry-run", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("acme/foo"), "{stdout}");
    assert!(stdout.contains("acme/bar"), "transitive missing: {stdout}");
    assert!(stdout.contains("2.3.0"), "must pick highest matching bar: {stdout}");
}

#[test]
fn dry_run_reports_unsatisfiable_constraint() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    // Only 0.x published; root requires ^1.
    let foo = p2_body("acme/foo", &[("0.9.0", "{}")]);

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

    write_composer_json(
        proj.path(),
        r#"{"name":"test/p","require":{"acme/foo":"^1.0"}}"#,
    );

    let output = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "update", "--dry-run", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");

    assert!(!output.status.success(), "expected failure exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("acme/foo"), "{stderr}");
    assert!(
        stderr.contains("no valid dependency resolution") || stderr.contains("no solution"),
        "{stderr}",
    );
}

#[test]
fn update_writes_composer_lock_to_project_root() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    let foo = p2_body("acme/foo", &[("1.5.0", "{}"), ("1.0.0", "{}")]);

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

    write_composer_json(
        proj.path(),
        r#"{"name":"test/p","require":{"acme/foo":"^1.0"}}"#,
    );
    let lock_path = proj.path().join("composer.lock");
    assert!(!lock_path.exists(), "lock should not exist before update");

    let output = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "update", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(lock_path.is_file(), "composer.lock was not written");

    // The lockfile must parse back through bougie's reader. This is
    // the structural sanity check.
    let lock = bougie_composer::lockfile::Lock::read(&lock_path)
        .expect("written lock must parse");
    assert_eq!(lock.packages.len(), 1);
    assert_eq!(lock.packages[0].name, "acme/foo");
    assert_eq!(lock.packages[0].version, "1.5.0");
    assert!(lock.content_hash.is_some(), "content-hash must be set");
}

#[test]
fn update_partitions_packages_into_prod_and_dev() {
    // composer.json has prod require for acme/foo and dev require
    // for acme/bar. Both are top-level requires; the lock must
    // place foo in `packages` and bar in `packages-dev`.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();

    let foo = p2_body("acme/foo", &[("1.0.0", "{}")]);
    let bar = p2_body("acme/bar", &[("2.0.0", "{}")]);

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

    write_composer_json(
        proj.path(),
        r#"{
            "name":"test/p",
            "require":{"acme/foo":"^1.0"},
            "require-dev":{"acme/bar":"^2.0"}
        }"#,
    );

    let output = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "update", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr),
    );

    let lock = bougie_composer::lockfile::Lock::read(&proj.path().join("composer.lock"))
        .unwrap();
    let prod_names: Vec<&str> =
        lock.packages.iter().map(|p| p.name.as_str()).collect();
    let dev_names: Vec<&str> =
        lock.packages_dev.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(prod_names, vec!["acme/foo"], "{prod_names:?}");
    assert_eq!(dev_names, vec!["acme/bar"], "{dev_names:?}");
}

#[test]
fn lock_written_by_update_passes_lock_verify() {
    // End-to-end: `bougie composer update` writes a lock, then
    // `bougie composer install --lock-verify` against the same
    // project (no network needed for the verify path — it's purely
    // structural) returns 0.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    let foo = p2_body("acme/foo", &[("1.0.0", "{}")]);

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

    write_composer_json(
        proj.path(),
        r#"{"name":"test/p","require":{"acme/foo":"^1.0"}}"#,
    );

    let update = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["composer", "update", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie update");
    assert!(
        update.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&update.stderr),
    );

    let verify = env
        .bougie()
        .args(["composer", "install", "--lock-verify", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie install --lock-verify");
    assert!(
        verify.status.success(),
        "lock-verify rejected our lockfile: stderr={} stdout={}",
        String::from_utf8_lossy(&verify.stderr),
        String::from_utf8_lossy(&verify.stdout),
    );
}

#[test]
fn update_without_composer_json_errors_clearly() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    // No composer.json staged.

    let output = env
        .bougie()
        .args(["composer", "update", "--dry-run", "-d"])
        .arg(proj.path())
        .output()
        .expect("run bougie");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("composer.json"), "{stderr}");
    assert!(stderr.contains("not a Composer project"), "{stderr}");
}

/// Use the `Command` API import so `cargo build -p bougie --tests`
/// flags accidental removals during refactors.
#[allow(dead_code)]
fn _ensure_command_imported(_: Command) {}
