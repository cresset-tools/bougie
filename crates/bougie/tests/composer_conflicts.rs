//! End-to-end tests for how `bougie lock` / `bougie composer update`
//! report a resolution blocked by a package's `conflict` declaration.
//!
//! The bug: a renamed package declaring `"conflict": {"old/name": "*"}`
//! made bougie say the *new* package had "no available version", which
//! pointed at repositories and stability flags instead of the conflict.

use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::TestEnv;

/// The bug report's layout: `pkgs/old`, `pkgs/new` as path repositories,
/// and `app/` requiring both names. Returns the `app` directory.
fn stage_rename_repro(root: &Path, app_require: &str) -> std::path::PathBuf {
    let write = |rel: &str, body: &str| {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    write(
        "pkgs/old/composer.json",
        r#"{"name": "example/old-name", "version": "1.0.0"}"#,
    );
    write(
        "pkgs/new/composer.json",
        r#"{"name": "example/new-name", "version": "2.0.0", "conflict": {"example/old-name": "*"}}"#,
    );
    write(
        "app/composer.json",
        &format!(
            r#"{{
                "name": "example/app",
                "repositories": [{{"type": "path", "url": "../pkgs/*"}}],
                "require": {app_require}
            }}"#
        ),
    );
    root.join("app")
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// An empty Packagist stand-in so nothing reaches the network; every
/// package in these tests comes from path repositories or explicit
/// mounts.
fn empty_packagist(rt: &tokio::runtime::Runtime) -> MockServer {
    rt.block_on(MockServer::start())
}

fn assert_names_the_rename_conflict(stderr: &str) {
    assert!(
        stderr.contains("example/new-name 2.0.0 conflicts with example/old-name 1.0.0."),
        "{stderr}"
    );
    assert!(
        stderr.contains("Root composer.json requires example/old-name *"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Root composer.json requires example/new-name *"),
        "{stderr}"
    );
    assert!(!stderr.contains("no available version"), "{stderr}");
}

#[test]
fn composer_update_names_a_rename_conflict() {
    let env = TestEnv::new();
    let tmp = TempDir::new().unwrap();
    let app = stage_rename_repro(
        tmp.path(),
        r#"{"example/old-name": "*", "example/new-name": "*"}"#,
    );
    let rt = rt();
    let packagist = empty_packagist(&rt);

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", packagist.uri())
        .args(["composer", "update", "--no-install", "-d"])
        .arg(&app)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "expected failure: {stderr}");
    assert_names_the_rename_conflict(&stderr);
    assert!(
        !app.join("composer.lock").exists(),
        "a failed resolve must not write a lock"
    );
}

#[test]
fn composer_update_dry_run_names_a_rename_conflict() {
    let env = TestEnv::new();
    let tmp = TempDir::new().unwrap();
    let app = stage_rename_repro(
        tmp.path(),
        r#"{"example/old-name": "*", "example/new-name": "*"}"#,
    );
    let rt = rt();
    let packagist = empty_packagist(&rt);

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", packagist.uri())
        .args(["composer", "update", "--dry-run", "-d"])
        .arg(&app)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "expected failure: {stderr}");
    assert_names_the_rename_conflict(&stderr);
}

#[test]
fn composer_update_locks_the_new_name_alone() {
    // The conflict must not block the renamed package on its own.
    let env = TestEnv::new();
    let tmp = TempDir::new().unwrap();
    let app = stage_rename_repro(tmp.path(), r#"{"example/new-name": "*"}"#);
    let rt = rt();
    let packagist = empty_packagist(&rt);

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", packagist.uri())
        .args(["composer", "update", "--no-install", "-d"])
        .arg(&app)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lock = bougie_composer::lockfile::Lock::read(&app.join("composer.lock")).unwrap();
    let names: Vec<_> = lock.packages.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["example/new-name"]);
}

/// A Packagist `/p2/` body. `extras` is spliced into every entry.
fn p2(name: &str, versions: &[(&str, &str)]) -> String {
    let entries: Vec<String> = versions
        .iter()
        .map(|(v, extras)| {
            format!(
                r#"{{"name":"{name}","version":"{v}","version_normalized":"{v}.0",
                    "type":"library",
                    "dist":{{"type":"zip","url":"https://e/{name}/{v}.zip","shasum":"aa"}}
                    {extras}}}"#
            )
        })
        .collect();
    format!(r#"{{"packages":{{"{name}":[{}]}}}}"#, entries.join(","))
}

fn mount(rt: &tokio::runtime::Runtime, server: &MockServer, name: &str, body: String) {
    rt.block_on(
        Mock::given(method("GET"))
            .and(wm_path(format!("/p2/{name}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server),
    );
}

#[test]
fn composer_update_names_an_indirect_conflict() {
    // acme/old arrives through acme/user, so acme/new's conflict with it
    // is only caught after the solve. The re-solve used to report
    // acme/new as having no available version.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    let rt = rt();
    let server = empty_packagist(&rt);
    mount(&rt, &server, "acme/old", p2("acme/old", &[("1.0.0", "")]));
    mount(
        &rt,
        &server,
        "acme/user",
        p2(
            "acme/user",
            &[("1.0.0", r#","require":{"acme/old":"^1.0"}"#)],
        ),
    );
    mount(
        &rt,
        &server,
        "acme/new",
        p2("acme/new", &[("2.0.0", r#","conflict":{"acme/old":"*"}"#)]),
    );
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"name":"test/p","require":{"acme/new":"*","acme/user":"*"}}"#,
    )
    .unwrap();

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", server.uri())
        .args(["composer", "update", "--no-install", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "expected failure: {stderr}");
    assert!(stderr.contains("conflicts with acme/old 1.0.0"), "{stderr}");
    assert!(!stderr.contains("no available version"), "{stderr}");
    assert!(!proj.path().join("composer.lock").exists());
}

#[test]
fn composer_update_falls_back_past_an_indirect_conflict() {
    // Same layout, but acme/new 1.0.0 has no conflict: lock it.
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    let rt = rt();
    let server = empty_packagist(&rt);
    mount(&rt, &server, "acme/old", p2("acme/old", &[("1.0.0", "")]));
    mount(
        &rt,
        &server,
        "acme/user",
        p2(
            "acme/user",
            &[("1.0.0", r#","require":{"acme/old":"^1.0"}"#)],
        ),
    );
    mount(
        &rt,
        &server,
        "acme/new",
        p2(
            "acme/new",
            &[("2.0.0", r#","conflict":{"acme/old":"*"}"#), ("1.0.0", "")],
        ),
    );
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"name":"test/p","require":{"acme/new":"*","acme/user":"*"}}"#,
    )
    .unwrap();

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", server.uri())
        .args(["composer", "update", "--no-install", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lock = bougie_composer::lockfile::Lock::read(&proj.path().join("composer.lock")).unwrap();
    let new = lock
        .packages
        .iter()
        .find(|p| p.name == "acme/new")
        .expect("acme/new locked");
    assert_eq!(new.version, "1.0.0");
}
