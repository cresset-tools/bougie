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

/// A diamond (root → a/a & b/b → shared/c → leaf/d) makes `shared/c`'s
/// subtree reachable by two paths. `bougie tree` (uv-style) must expand
/// it once and collapse the repeat to `(*)`; the path-local cycle guard
/// alone would re-expand every shared subtree per path, which blows up
/// combinatorially and hung `tree` on real lockfiles. `composer show
/// --tree` stays Composer-exact and repeats the subtree in full.
const DIAMOND_LOCK: &str = r#"{"content-hash":"x","packages":[
    {"name":"a/a","version":"1.0.0","require":{"shared/c":"^1.0"}},
    {"name":"b/b","version":"1.0.0","require":{"shared/c":"^1.0"}},
    {"name":"shared/c","version":"1.0.0","require":{"leaf/d":"^1.0"}},
    {"name":"leaf/d","version":"1.0.0"}
],"packages-dev":[]}"#;

#[test]
fn tree_dedupes_shared_subtrees() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    write_composer_json(
        proj.path(),
        r#"{"name":"test/p","require":{"a/a":"^1.0","b/b":"^1.0"}}"#,
    );
    std::fs::write(proj.path().join("composer.lock"), DIAMOND_LOCK).unwrap();

    let out = env.bougie().args(["tree", "-d"]).arg(proj.path()).output().unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    // The deepest leaf is rendered exactly once — the second path to
    // shared/c collapses before reaching it.
    assert_eq!(s.matches("leaf/d ^1.0").count(), 1, "leaf must render once: {s}");
    assert!(s.contains("shared/c ^1.0 (*)"), "repeat collapses to (*): {s}");
    assert!(s.contains("(*) Package tree already displayed"), "legend: {s}");
}

#[test]
fn composer_show_tree_keeps_full_repeat() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    write_composer_json(
        proj.path(),
        r#"{"name":"test/p","require":{"a/a":"^1.0","b/b":"^1.0"}}"#,
    );
    std::fs::write(proj.path().join("composer.lock"), DIAMOND_LOCK).unwrap();

    let out = env
        .bougie()
        .args(["composer", "show", "--tree", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    // Composer-exact: shared/c's subtree is repeated under both paths.
    assert_eq!(s.matches("leaf/d ^1.0").count(), 2, "must repeat in full: {s}");
    assert!(!s.contains("(*)"), "no dedupe marker in compat mode: {s}");
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
fn lock_already_in_sync_is_offline_noop() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    let composer_json = r#"{"name":"test/p","require":{"acme/foo":"^1.0"}}"#;
    write_composer_json(proj.path(), composer_json);
    // Lock carries the *correct* content-hash for the composer.json above.
    let hash = bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap();
    let lock = format!(
        r#"{{"content-hash":"{hash}","packages":[
            {{"name":"acme/foo","version":"1.2.0","version_normalized":"1.2.0.0"}}
        ],"packages-dev":[]}}"#
    );
    std::fs::write(proj.path().join("composer.lock"), &lock).unwrap();
    let before = std::fs::read_to_string(proj.path().join("composer.lock")).unwrap();

    // No mock server: an in-sync lock must not touch the network.
    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", "http://127.0.0.1:1/none")
        .args(["lock", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("already in sync"), "{s}");
    // Lock unchanged.
    assert_eq!(std::fs::read_to_string(proj.path().join("composer.lock")).unwrap(), before);
}

#[test]
fn lock_constraint_change_reresolves_only_that_package() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    // composer.json bumps acme/foo's constraint past the locked 1.0.0;
    // acme/bar's ^2.0 still matches its locked 2.0.0.
    write_composer_json(
        proj.path(),
        r#"{"name":"test/p","require":{"acme/foo":"^1.5","acme/bar":"^2.0"}}"#,
    );
    // Stale content-hash (bogus) → lock proceeds to reconcile.
    std::fs::write(
        proj.path().join("composer.lock"),
        r#"{"content-hash":"stale","packages":[
            {"name":"acme/foo","version":"1.0.0","version_normalized":"1.0.0.0"},
            {"name":"acme/bar","version":"2.0.0","version_normalized":"2.0.0.0"}
        ],"packages-dev":[]}"#,
    )
    .unwrap();

    let foo = p2_body("acme/foo", &["1.6.0", "1.5.0", "1.0.0"]);
    let bar = p2_body("acme/bar", &["2.0.0"]);
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

    let out = env
        .bougie()
        .env("BOUGIE_PACKAGIST_BASE_URL", &uri)
        .args(["lock", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));

    let after = std::fs::read_to_string(proj.path().join("composer.lock")).unwrap();
    // acme/foo re-resolved to the highest in ^1.5 (1.6.0); acme/bar held.
    assert!(after.contains("\"1.6.0\""), "foo should move to 1.6.0: {after}");
    assert!(after.contains("acme/bar") && after.contains("\"2.0.0\""), "bar stays pinned: {after}");
    // never installs.
    assert!(!proj.path().join("vendor").exists());
}

#[test]
fn lock_dry_run_writes_nothing() {
    let env = TestEnv::new();
    let proj = TempDir::new().unwrap();
    write_composer_json(proj.path(), r#"{"name":"test/p","require":{"acme/foo":"^1.5"}}"#);
    std::fs::write(
        proj.path().join("composer.lock"),
        r#"{"content-hash":"stale","packages":[
            {"name":"acme/foo","version":"1.0.0","version_normalized":"1.0.0.0"}
        ],"packages-dev":[]}"#,
    )
    .unwrap();
    let before = std::fs::read_to_string(proj.path().join("composer.lock")).unwrap();

    let foo = p2_body("acme/foo", &["1.6.0", "1.0.0"]);
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
        .args(["lock", "--dry-run", "-d"])
        .arg(proj.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    // Lock untouched by --dry-run.
    assert_eq!(std::fs::read_to_string(proj.path().join("composer.lock")).unwrap(), before);
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
