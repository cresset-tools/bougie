//! Integration tests for [`ResolveProvider`].
//!
//! Each test stands up a wiremock server with one or more `/p2/`
//! responses, builds a `ResolveProvider` from a fixture
//! `composer.json`, and runs `pubgrub::resolve` end-to-end. The
//! assertions inspect the solution set (which package versions
//! pubgrub picked) and, for failure paths, the `NoSolution`
//! derivation tree.

use super::*;
use bougie_paths::Paths;
use pubgrub::{resolve, DefaultStringReporter, PubGrubError, Reporter};
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn paths_in(tmp: &Path) -> Paths {
    let home = tmp.join("home");
    let cache = tmp.join("cache");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    Paths::new(home, cache)
}

/// One Packagist version-entry, fully expanded.
fn version_entry(
    name: &str,
    version: &str,
    require: serde_json::Value,
) -> serde_json::Value {
    json!({
        "name": name,
        "version": version,
        "version_normalized": format!("{version}.0"),
        "type": "library",
        "dist": {"type":"zip","url":format!("https://e/{name}/{version}.zip"),"shasum":"aa"},
        "require": require,
    })
}

/// Build a `/p2/<name>.json` body from a slice of (version, require) tuples.
fn p2_body(name: &str, versions: &[(&str, serde_json::Value)]) -> String {
    let entries: Vec<_> = versions
        .iter()
        .map(|(v, req)| version_entry(name, v, req.clone()))
        .collect();
    let doc = json!({
        "packages": { name: entries },
    });
    serde_json::to_string(&doc).unwrap()
}

/// Mount a `/p2/<name>.json` handler returning `body`. Inlined into
/// each test's `rt.block_on` async block.
async fn mount_p2(server: &MockServer, name: &str, body: String) {
    let p = format!("/p2/{name}.json");
    Mock::given(method("GET"))
        .and(wm_path(p))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

#[test]
fn resolves_single_dep_to_highest_in_range() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let body = p2_body(
        "acme/foo",
        &[
            ("2.0.0", json!({})),
            ("1.5.0", json!({})),
            ("1.2.0", json!({})),
            ("0.9.0", json!({})),
        ],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {"acme/foo": "^1.0"},
    });

    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo_version = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .expect("acme/foo should be resolved");
    assert_eq!(foo_version.to_string(), "1.5.0.0");
    assert_eq!(provider.cache_size(), 1, "only one package fetched");
}

#[test]
fn resolves_transitive_dependency() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let foo_body = p2_body(
        "acme/foo",
        &[("1.0.0", json!({"acme/bar": "^2.0"}))],
    );
    let bar_body = p2_body(
        "acme/bar",
        &[
            ("2.5.0", json!({})),
            ("2.1.0", json!({})),
            ("1.0.0", json!({})),
        ],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", foo_body).await;
        mount_p2(&server, "acme/bar", bar_body).await;
        (server.uri(), server)
    });

    let composer_json = json!({"require": {"acme/foo": "^1.0"}});
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let bar = solution
        .get(&PubGrubPackage::Package("acme/bar".into()))
        .expect("acme/bar should be resolved transitively");
    assert_eq!(bar.to_string(), "2.5.0.0");
}

#[test]
fn unsatisfiable_constraint_produces_no_solution() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    // Only 0.x is published; the root requires ^1.0.
    let body = p2_body("acme/foo", &[("0.9.0", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({"require": {"acme/foo": "^1.0"}});
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let err = resolve(&provider, PubGrubPackage::Root, root).unwrap_err();
    match err {
        PubGrubError::NoSolution(tree) => {
            let msg = DefaultStringReporter::report(&tree);
            assert!(msg.contains("acme/foo"), "{msg}");
        }
        other => panic!("expected NoSolution, got {other:?}"),
    }
}

#[test]
fn prerelease_versions_are_filtered_from_candidates() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    // Highest is a beta — should be skipped. The next stable, 1.5.0,
    // is the answer.
    let body = p2_body(
        "acme/foo",
        &[
            ("2.0.0-beta1", json!({})),
            ("1.5.0", json!({})),
            ("1.0.0", json!({})),
        ],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({"require": {"acme/foo": ">=1.0"}});
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .unwrap();
    assert_eq!(foo.to_string(), "1.5.0.0");
}

#[test]
fn platform_requires_are_skipped_no_fetch_attempted() {
    // Root requires php + acme/foo; the resolver should not try to
    // GET /p2/php.json (which would 404 since the mock server has no
    // handler for it).
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let body = p2_body("acme/foo", &[("1.0.0", json!({"php": ">=8.1"}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {
            "php": ">=8.1",
            "ext-mbstring": "*",
            "acme/foo": "^1.0",
        },
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution.get(&PubGrubPackage::Package("acme/foo".into())).is_some());
    assert!(solution.get(&PubGrubPackage::Package("php".into())).is_none());
    assert!(solution.get(&PubGrubPackage::Package("ext-mbstring".into())).is_none());
}

#[test]
fn require_dev_included_when_no_dev_is_false() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let foo = p2_body("acme/foo", &[("1.0.0", json!({}))]);
    let bar = p2_body("acme/bar", &[("2.0.0", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", foo).await;
        mount_p2(&server, "acme/bar", bar).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {"acme/foo": "^1.0"},
        "require-dev": {"acme/bar": "^2.0"},
    });

    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, /*no_dev=*/ false)
            .unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution.get(&PubGrubPackage::Package("acme/bar".into())).is_some());
}

#[test]
fn require_dev_excluded_when_no_dev_is_true() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let foo = p2_body("acme/foo", &[("1.0.0", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", foo).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {"acme/foo": "^1.0"},
        "require-dev": {"acme/bar": "^2.0"},
    });

    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, /*no_dev=*/ true)
            .unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution.get(&PubGrubPackage::Package("acme/bar".into())).is_none());
}

