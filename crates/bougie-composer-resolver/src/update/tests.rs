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

/// Build a `/p2/<name>.json` body where each version can declare
/// extra Composer maps (`replace`, `provide`, etc.). Inline JSON
/// would also work, but pulling this helper out keeps the
/// replace/provide tests below readable.
fn p2_body_with_extras(name: &str, versions: &[(&str, serde_json::Value, serde_json::Value)])
    -> String
{
    let entries: Vec<_> = versions
        .iter()
        .map(|(v, require, extras)| {
            let mut entry = json!({
                "name": name,
                "version": v,
                "version_normalized": format!("{v}.0"),
                "type": "library",
                "dist": {"type":"zip","url":format!("https://e/{name}/{v}.zip"),"shasum":"aa"},
                "require": require,
            });
            // Merge extras (`replace`, `provide`, etc.) into the entry.
            if let serde_json::Value::Object(extra_map) = extras {
                let obj = entry.as_object_mut().unwrap();
                for (k, v) in extra_map {
                    obj.insert(k.clone(), v.clone());
                }
            }
            entry
        })
        .collect();
    let doc = json!({"packages": { name: entries }});
    serde_json::to_string(&doc).unwrap()
}

#[test]
fn replace_forces_replaced_package_to_consistent_version() {
    // monolith@2.0.0 replaces sub@==2.0.0. Root requires both.
    // Resolver must pick sub@2.0.0 even though sub@2.5.0 exists.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let monolith = p2_body_with_extras(
        "acme/monolith",
        &[("2.0.0", json!({}), json!({"replace": {"acme/sub": "2.0.0"}}))],
    );
    let sub = p2_body(
        "acme/sub",
        &[("2.5.0", json!({})), ("2.0.0", json!({}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/monolith", monolith).await;
        mount_p2(&server, "acme/sub", sub).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {
            "acme/monolith": "^2.0",
            "acme/sub": "^2.0",
        },
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let sub_version = solution
        .get(&PubGrubPackage::Package("acme/sub".into()))
        .expect("acme/sub should be resolved");
    assert_eq!(sub_version.to_string(), "2.0.0.0", "replace must pin sub@2.0.0");
}

#[test]
fn provide_forces_consistent_version_same_as_replace() {
    // Provide encodes identically to replace in this resolver. Same
    // shape as the previous test but using the `provide` map.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let provider_pkg = p2_body_with_extras(
        "acme/provider",
        &[("1.0.0", json!({}), json!({"provide": {"acme/iface": "1.0.0"}}))],
    );
    let iface = p2_body(
        "acme/iface",
        &[("2.0.0", json!({})), ("1.0.0", json!({}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/provider", provider_pkg).await;
        mount_p2(&server, "acme/iface", iface).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {"acme/provider": "^1.0", "acme/iface": "^1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let iface_version = solution
        .get(&PubGrubPackage::Package("acme/iface".into()))
        .expect("acme/iface should be resolved");
    assert_eq!(iface_version.to_string(), "1.0.0.0");
}

#[test]
fn replace_conflicts_with_a_separate_require_yield_no_solution() {
    // monolith@1.0.0 replaces sub@==1.0.0, but root requires sub@^2.
    // These are incompatible — resolver must report NoSolution and
    // name the replaced package somewhere in the derivation tree.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let monolith = p2_body_with_extras(
        "acme/monolith",
        &[("1.0.0", json!({}), json!({"replace": {"acme/sub": "1.0.0"}}))],
    );
    let sub = p2_body("acme/sub", &[("2.0.0", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/monolith", monolith).await;
        mount_p2(&server, "acme/sub", sub).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {"acme/monolith": "^1.0", "acme/sub": "^2.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let err = resolve(&provider, PubGrubPackage::Root, root).unwrap_err();
    match err {
        pubgrub::PubGrubError::NoSolution(tree) => {
            let msg = DefaultStringReporter::report(&tree);
            assert!(msg.contains("acme/sub"), "{msg}");
        }
        other => panic!("expected NoSolution, got {other:?}"),
    }
}

#[test]
fn replace_clause_as_range_intersects_with_separate_require() {
    // monolith@2.0.0 declares `replace: { acme/sub: "^2.0" }`. Root
    // separately requires sub@^2.1 (a sub-range). The intersection
    // forces sub into ^2.1 — pubgrub picks 2.3.0, not 2.0.0.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let monolith = p2_body_with_extras(
        "acme/monolith",
        &[("2.0.0", json!({}), json!({"replace": {"acme/sub": "^2.0"}}))],
    );
    let sub = p2_body(
        "acme/sub",
        &[("2.3.0", json!({})), ("2.1.0", json!({})), ("2.0.0", json!({}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/monolith", monolith).await;
        mount_p2(&server, "acme/sub", sub).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {"acme/monolith": "^2.0", "acme/sub": "^2.1"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let sub_version = solution
        .get(&PubGrubPackage::Package("acme/sub".into()))
        .unwrap();
    assert_eq!(sub_version.to_string(), "2.3.0.0");
}

#[test]
fn platform_replace_clauses_are_skipped() {
    // polyfill@1.0.0 replaces php@8.0.x — typical polyfill-php80
    // pattern. Today (until issue #118 lands) platform packages are
    // filtered before they reach pubgrub, both in `require` and in
    // `replace`. This test pins that behavior: a polyfill declaring
    // replace.php must not crash or try to fetch `/p2/php.json`.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let polyfill = p2_body_with_extras(
        "symfony/polyfill-php80",
        &[("1.0.0", json!({}), json!({"replace": {"php": "8.0.x"}}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "symfony/polyfill-php80", polyfill).await;
        // Deliberately no /p2/php.json handler — if the resolver
        // tries to fetch it, the mock returns 404 and the test fails.
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {"symfony/polyfill-php80": "^1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(
        solution
            .get(&PubGrubPackage::Package("symfony/polyfill-php80".into()))
            .is_some(),
    );
    // php must not appear in the solution — it was filtered.
    assert!(solution.get(&PubGrubPackage::Package("php".into())).is_none());
}

#[test]
fn default_minimum_stability_is_stable_unchanged_behavior() {
    // Composer's default is `stable`. Resolver should reject the
    // beta even though it's the highest version.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let body = p2_body(
        "acme/foo",
        &[
            ("2.0.0-beta1", json!({})),
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
    assert_eq!(foo.to_string(), "1.0.0.0", "beta must be filtered by default");
}

#[test]
fn minimum_stability_dev_allows_dev_versions() {
    // Only a `-dev` suffixed version is published. With the default
    // stable gate this would be filtered; minimum-stability=dev must
    // let it through.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    // Pick a non-boundary version: 1.5.0-dev sits comfortably
    // inside [1.0, 2.0) regardless of how the lower-bound marker
    // is encoded (composer's partial-constraint rule synthesizes a
    // stable lower bound for `^1.0`, so a 1.0.0-dev candidate would
    // be excluded for unrelated reasons — see PR #115).
    let body = p2_body("acme/devonly", &[("1.5.0-dev", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/devonly", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "minimum-stability": "dev",
        "require": {"acme/devonly": "^1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution
        .get(&PubGrubPackage::Package("acme/devonly".into()))
        .is_some());
}

#[test]
fn minimum_stability_beta_accepts_beta_rejects_alpha() {
    // Versions: 2.0-alpha1 (filtered), 1.5-beta1 (acceptable),
    // 1.0 (stable, acceptable). Without floor: pick 2.0-alpha1.
    // With minimum-stability=beta: pick 1.5-beta1 since it's
    // higher than 1.0 stable and 2.0-alpha1 is below the floor.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let body = p2_body(
        "acme/foo",
        &[
            ("2.0.0-alpha1", json!({})),
            ("1.5.0-beta1", json!({})),
            ("1.0.0", json!({})),
        ],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "minimum-stability": "beta",
        "require": {"acme/foo": ">=1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .unwrap();
    assert_eq!(foo.to_string(), "1.5.0.0-beta1", "got {}", foo);
}

#[test]
fn per_package_at_dev_flag_overrides_default_stability() {
    // Global default is `stable` (composer.json has no
    // minimum-stability). acme/foo publishes only a -dev version;
    // the root require carries `@dev` for acme/foo, so dev is
    // acceptable for THIS package — bypassing the global gate.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let body = p2_body("acme/foo", &[("1.5.0-dev", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {"acme/foo": "^1.0@dev"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .is_some());
}

#[test]
fn per_package_at_stable_flag_tightens_a_global_dev_floor() {
    // Global is `dev`, so anything goes by default. acme/strict
    // carries `@stable` — it's restricted to stable candidates even
    // though the global floor is below. Other packages (acme/loose)
    // get the global floor.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let strict = p2_body(
        "acme/strict",
        &[("2.0.0-beta1", json!({})), ("1.0.0", json!({}))],
    );
    let loose = p2_body(
        "acme/loose",
        &[("2.0.0-beta1", json!({})), ("1.0.0", json!({}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/strict", strict).await;
        mount_p2(&server, "acme/loose", loose).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "minimum-stability": "dev",
        "require": {
            "acme/strict": "*@stable",
            "acme/loose": "*",
        },
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let strict = solution
        .get(&PubGrubPackage::Package("acme/strict".into()))
        .unwrap();
    let loose = solution
        .get(&PubGrubPackage::Package("acme/loose".into()))
        .unwrap();
    assert_eq!(strict.to_string(), "1.0.0.0", "strict must drop the beta");
    assert_eq!(loose.to_string(), "2.0.0.0-beta1", "loose can keep the beta");
}

#[test]
fn unknown_minimum_stability_value_is_a_build_error() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let composer_json = json!({
        "minimum-stability": "wonky",
        "require": {},
    });
    let client = crate::metadata::build_client().unwrap();
    let err = ResolveProvider::build(client, paths, "http://x".into(), &composer_json, true)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("wonky"), "{msg}");
    assert!(msg.contains("stable"), "{msg}");
}

/// Helper: mount the dev variant explicitly (`/p2/<name>~dev.json`).
async fn mount_p2_dev(server: &MockServer, name: &str, body: String) {
    let p = format!("/p2/{name}~dev.json");
    Mock::given(method("GET"))
        .and(wm_path(p))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

#[test]
fn dev_floor_pulls_branch_versions_from_tilde_dev_json() {
    // The stable doc has only an old 1.0.0; ~dev.json carries the
    // 1.x-dev branch which (with floor=dev) is the highest in-range
    // candidate.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let stable = p2_body("acme/foo", &[("1.0.0", json!({}))]);
    let dev = p2_body("acme/foo", &[("1.x-dev", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", stable).await;
        mount_p2_dev(&server, "acme/foo", dev).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "minimum-stability": "dev",
        "require": {"acme/foo": "^1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .unwrap();
    // 1.x-dev normalizes to 1.9999999... which is higher than 1.0.0.
    assert_eq!(foo.to_string(), "1.9999999.9999999.9999999-dev", "got {foo}");
}

#[test]
fn dev_floor_tolerates_missing_tilde_dev_json() {
    // ~dev.json 404s (we don't mount it). The resolver should still
    // succeed using only the stable doc — Packagist 404s the dev
    // variant for any package with no branches, which is common.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let stable = p2_body(
        "acme/foo",
        &[("1.2.0-dev", json!({})), ("1.0.0", json!({}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", stable).await;
        // Deliberately no ~dev.json mock — wiremock returns 404 by
        // default, which the optional fetcher must absorb as
        // "no dev candidates."
        (server.uri(), server)
    });

    let composer_json = json!({
        "minimum-stability": "dev",
        "require": {"acme/foo": "^1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .unwrap();
    // The dev-suffixed numeric in the stable doc is still admitted —
    // it's higher than 1.0.0.
    assert_eq!(foo.to_string(), "1.2.0.0-dev");
}

#[test]
fn stable_floor_does_not_fetch_tilde_dev_json() {
    // Default stable floor must NOT consult ~dev.json. We assert
    // this by mounting a 500-response there: if the resolver ever
    // calls the URL, the test fails with a server-error.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let stable = p2_body("acme/foo", &[("1.0.0", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", stable).await;
        // Trap mock: any GET to ~dev.json is a 500.
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/foo~dev.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let composer_json = json!({"require": {"acme/foo": "^1.0"}});
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .unwrap();
    assert_eq!(foo.to_string(), "1.0.0.0");
}

#[test]
fn combined_stable_plus_dev_versions_sort_descending() {
    // Both docs carry versions in the constraint's range. The
    // resolver must pick the *highest* across both — 2.0.0 from
    // stable beats 1.x-dev (normalized 1.99...) from dev when both
    // satisfy `>=1.0`.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let stable = p2_body("acme/foo", &[("2.0.0", json!({})), ("1.0.0", json!({}))]);
    let dev = p2_body("acme/foo", &[("1.x-dev", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", stable).await;
        mount_p2_dev(&server, "acme/foo", dev).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "minimum-stability": "dev",
        "require": {"acme/foo": ">=1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .unwrap();
    assert_eq!(foo.to_string(), "2.0.0.0", "stable 2.0.0 > dev 1.x-dev");
}

// ===================== Virtual packages =====================

/// Tiny project + provider scenario for virtual-package tests.
/// `provider` declares it provides `virtual_name` at `provided`;
/// `consumer` requires `virtual_name` somewhere in its graph.
fn build_virtual_scenario(
    provider_name: &str,
    provider_version: &str,
    virtual_name: &str,
    virtual_provided: &str,
    consumer_name: &str,
    consumer_version: &str,
    consumer_requires_virtual: &str,
) -> (String, String) {
    let provider_doc = p2_body_with_extras(
        provider_name,
        &[(
            provider_version,
            json!({}),
            json!({"provide": {virtual_name: virtual_provided}}),
        )],
    );
    let consumer_doc = p2_body(
        consumer_name,
        &[(consumer_version, json!({virtual_name: consumer_requires_virtual}))],
    );
    (provider_doc, consumer_doc)
}

#[test]
fn virtual_package_satisfied_by_provide_clause() {
    // The canonical psr/http-client-implementation pattern:
    // guzzle provides the virtual, some-client requires it.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let (guzzle, some_client) = build_virtual_scenario(
        "acme/guzzle",
        "7.4.0",
        "acme/http-impl",
        "1.0",
        "acme/some-client",
        "1.0.0",
        "^1.0",
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/guzzle", guzzle).await;
        mount_p2(&server, "acme/some-client", some_client).await;
        // Deliberately no /p2/acme/http-impl.json mock — wiremock
        // returns 404, which is exactly what Packagist does for a
        // virtual name. The resolver must satisfy it from the
        // provider's `provide` clause instead.
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {
            "acme/guzzle": "^7.0",
            "acme/some-client": "^1.0",
        },
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    provider.pre_fetch_closure().unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    // Both real packages are in the solution.
    assert!(solution
        .get(&PubGrubPackage::Package("acme/guzzle".into()))
        .is_some());
    assert!(solution
        .get(&PubGrubPackage::Package("acme/some-client".into()))
        .is_some());
    // The virtual itself is in pubgrub's raw solution but the
    // dry-run / lockfile output filters it out (see the orchestrator
    // tests below). Confirm the virtual selection map has it.
    assert!(
        !provider.virtual_selections.borrow().is_empty(),
        "virtual_selections should record the resolution",
    );
}

#[test]
fn virtual_package_with_no_provider_yields_no_solution() {
    // Project requires a virtual nobody provides. Pubgrub must
    // produce NoSolution, not crash.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let lonely = p2_body(
        "acme/lonely",
        &[("1.0.0", json!({"acme/missing-impl": "^1.0"}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/lonely", lonely).await;
        // No provider for acme/missing-impl, and no Packagist entry.
        (server.uri(), server)
    });

    let composer_json = json!({"require": {"acme/lonely": "^1.0"}});
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    provider.pre_fetch_closure().unwrap();
    let root = provider.root_version();

    let err = resolve(&provider, PubGrubPackage::Root, root).unwrap_err();
    matches!(err, pubgrub::PubGrubError::NoSolution(_))
        .then_some(())
        .expect("expected NoSolution");
}

#[test]
fn replace_clause_with_range_constraint_registers_virtual() {
    // Real-world pattern from magento/zend-cache (replaces
    // zfs1/zend-cache: ^1.12). The replace clause is a range, not a
    // bare version — our parser falls back to the replacer's own
    // version as the synthetic candidate.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let monolith = p2_body_with_extras(
        "acme/big-cache",
        &[(
            "1.16.0",
            json!({}),
            json!({"replace": {"acme/legacy-cache": "^1.12"}}),
        )],
    );
    // Something requires the legacy cache somewhere in the graph.
    let consumer = p2_body(
        "acme/consumer",
        &[("1.0.0", json!({"acme/legacy-cache": "^1.12"}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/big-cache", monolith).await;
        mount_p2(&server, "acme/consumer", consumer).await;
        // No /p2/acme/legacy-cache.json — only the replace clause
        // makes it resolvable.
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {
            "acme/big-cache": "^1.16",
            "acme/consumer": "^1.0",
        },
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    provider.pre_fetch_closure().unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution
        .get(&PubGrubPackage::Package("acme/big-cache".into()))
        .is_some());
    assert!(solution
        .get(&PubGrubPackage::Package("acme/consumer".into()))
        .is_some());
}

#[test]
fn multiple_providers_at_same_virtual_version_dedup() {
    // Two providers offer the same virtual at the same version.
    // versions_for must return a single virtual candidate, not two,
    // and virtual_selections records exactly one mapping
    // (first-write-wins).
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let p1 = p2_body_with_extras(
        "acme/p1",
        &[("1.0.0", json!({}), json!({"provide": {"acme/iface": "1.0.0"}}))],
    );
    let p2 = p2_body_with_extras(
        "acme/p2",
        &[("2.0.0", json!({}), json!({"provide": {"acme/iface": "1.0.0"}}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/p1", p1).await;
        mount_p2(&server, "acme/p2", p2).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {
            "acme/p1": "^1.0",
            "acme/p2": "^2.0",
            "acme/iface": "^1.0",
        },
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    provider.pre_fetch_closure().unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    // Both providers are explicitly required by root.
    assert!(solution
        .get(&PubGrubPackage::Package("acme/p1".into()))
        .is_some());
    assert!(solution
        .get(&PubGrubPackage::Package("acme/p2".into()))
        .is_some());
    // Only one virtual selection mapping exists for acme/iface@1.0.0.
    let selections = provider.virtual_selections.borrow();
    let iface_entries: Vec<_> = selections
        .keys()
        .filter(|(name, _)| name == "acme/iface")
        .collect();
    assert_eq!(iface_entries.len(), 1);
}

// ===================== prefer-stable =====================

#[test]
fn prefer_stable_picks_stable_over_higher_beta() {
    // minimum-stability=beta opens betas; prefer-stable=true says
    // "use stable if any matches in range." 1.0.0 stable wins over
    // 2.0.0-beta1 even though the beta is higher.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let body = p2_body(
        "acme/foo",
        &[("2.0.0-beta1", json!({})), ("1.0.0", json!({}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "minimum-stability": "beta",
        "prefer-stable": true,
        "require": {"acme/foo": ">=1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .unwrap();
    assert_eq!(foo.to_string(), "1.0.0.0", "expected stable; got {foo}");
}

#[test]
fn without_prefer_stable_highest_beta_still_wins() {
    // Same fixture, prefer-stable disabled → beta wins.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let body = p2_body(
        "acme/foo",
        &[("2.0.0-beta1", json!({})), ("1.0.0", json!({}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "minimum-stability": "beta",
        "require": {"acme/foo": ">=1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .unwrap();
    assert_eq!(foo.to_string(), "2.0.0.0-beta1");
}

#[test]
fn prefer_stable_falls_back_to_unstable_when_no_stable_in_range() {
    // Only betas available in range. prefer-stable can't find a
    // stable match, so it falls back to the highest in range.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let body = p2_body(
        "acme/foo",
        &[("2.0.0-beta2", json!({})), ("2.0.0-beta1", json!({}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "minimum-stability": "beta",
        "prefer-stable": true,
        // Explicit beta-anchored caret so the lower bound admits
        // betas — the synthesized lower bound from `^2.0` would
        // be `2.0.0-stable` and would exclude `2.0.0-beta1`.
        "require": {"acme/foo": "^2.0.0-beta1"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .unwrap();
    assert_eq!(foo.to_string(), "2.0.0.0-beta2", "fallback to highest");
}

#[test]
fn prefer_stable_is_noop_when_floor_is_stable() {
    // minimum-stability=stable (default) means every candidate is
    // already stable. prefer-stable is a no-op; the resolver picks
    // the highest in range as usual.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let body = p2_body(
        "acme/foo",
        &[("2.0.0", json!({})), ("1.0.0", json!({}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "prefer-stable": true,
        "require": {"acme/foo": ">=1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, uri, &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .unwrap();
    assert_eq!(foo.to_string(), "2.0.0.0");
}

#[test]
fn unknown_prefer_stable_type_is_a_build_error() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let composer_json = json!({
        "prefer-stable": "yes",
        "require": {},
    });
    let client = crate::metadata::build_client().unwrap();
    let err = ResolveProvider::build(client, paths, "http://x".into(), &composer_json, true)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("prefer-stable"), "{msg}");
}

