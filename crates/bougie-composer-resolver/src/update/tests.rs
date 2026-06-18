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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let bar = solution
        .get(&PubGrubPackage::Package("acme/bar".into()))
        .expect("acme/bar should be resolved transitively");
    assert_eq!(bar.to_string(), "2.5.0.0");
}

#[test]
fn lowest_strategy_picks_lowest_in_range() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    // Same fixture as `resolves_single_dep_to_highest_in_range`, which
    // resolves `^1.0` to 1.5.0; under `lowest` it must pick 1.2.0.
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

    let composer_json = json!({"require": {"acme/foo": "^1.0"}});
    let client = crate::metadata::build_client().unwrap();
    let mut provider =
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
    provider.set_resolution(ResolutionStrategy::Lowest);
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .expect("acme/foo should be resolved");
    assert_eq!(foo.to_string(), "1.2.0.0");
}

#[test]
fn lowest_direct_lowers_direct_but_keeps_transitive_highest() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    // acme/foo is a *direct* require with two in-range versions; acme/bar
    // is only reachable transitively. Under `lowest-direct`, foo drops to
    // its lowest in-range (1.0.0) while bar stays at its highest (2.5.0).
    let foo_body = p2_body(
        "acme/foo",
        &[
            ("1.5.0", json!({"acme/bar": "^2.0"})),
            ("1.0.0", json!({"acme/bar": "^2.0"})),
        ],
    );
    let bar_body = p2_body(
        "acme/bar",
        &[("2.5.0", json!({})), ("2.1.0", json!({}))],
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
    let mut provider =
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
    provider.set_resolution(ResolutionStrategy::LowestDirect);
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .expect("acme/foo should be resolved");
    let bar = solution
        .get(&PubGrubPackage::Package("acme/bar".into()))
        .expect("acme/bar should be resolved transitively");
    assert_eq!(foo.to_string(), "1.0.0.0", "direct dep lowered");
    assert_eq!(bar.to_string(), "2.5.0.0", "transitive dep stays highest");
}

#[test]
fn lowest_strategy_lowers_transitive_too() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    // Same fixture as the lowest-direct test, but full `lowest` lowers the
    // transitive bar as well (2.1.0 rather than 2.5.0).
    let foo_body = p2_body("acme/foo", &[("1.0.0", json!({"acme/bar": "^2.0"}))]);
    let bar_body = p2_body(
        "acme/bar",
        &[("2.5.0", json!({})), ("2.1.0", json!({}))],
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
    let mut provider =
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
    provider.set_resolution(ResolutionStrategy::Lowest);
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let bar = solution
        .get(&PubGrubPackage::Package("acme/bar".into()))
        .expect("acme/bar should be resolved transitively");
    assert_eq!(bar.to_string(), "2.1.0.0");
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, /*no_dev=*/ false)
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, /*no_dev=*/ true)
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
fn fork_replace_back_edge_to_replacer_does_not_break_solve() {
    // The Mage-OS fork layout. `acme/fork` is a fork of `acme/orig`:
    //   fork@2.0.0  replace { acme/orig: 1.0.0 }   (this fork *is* orig 1.0.0)
    //   orig@1.0.0  require { acme/fork: 2.1.0 }    (the original's sole dep
    //                                                is a back-edge to a
    //                                                *different* fork version)
    // The project pins fork@2.0.0. Composer never installs orig (it's
    // replaced by fork), so orig's back-edge to fork@2.1.0 never applies.
    // bougie used to pull orig into the solve and let that back-edge force
    // fork@2.1.0, contradicting the pinned fork@2.0.0 → spurious
    // NoSolution. The `replaced_by` filter in `compute_parsed_deps` drops
    // orig's edge to its own replacer, so the solve succeeds with
    // fork@2.0.0.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let fork = p2_body_with_extras(
        "acme/fork",
        &[
            ("2.0.0", json!({}), json!({"replace": {"acme/orig": "1.0.0"}})),
            ("2.1.0", json!({}), json!({"replace": {"acme/orig": "1.0.0"}})),
        ],
    );
    let orig = p2_body_with_extras(
        "acme/orig",
        &[("1.0.0", json!({"acme/fork": "2.1.0"}), json!({}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/fork", fork).await;
        mount_p2(&server, "acme/orig", orig).await;
        (server.uri(), server)
    });

    // Pin the fork at 2.0.0 and pull the original into the graph (as the
    // real resolve does via a transitive require on the original's name).
    let composer_json = json!({
        "require": {"acme/fork": "2.0.0", "acme/orig": "^1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root)
        .expect("fork-replace back-edge must not break the solve");
    let fork_version = solution
        .get(&PubGrubPackage::Package("acme/fork".into()))
        .expect("acme/fork should be resolved");
    assert_eq!(
        fork_version.to_string(),
        "2.0.0.0",
        "fork must stay at the pinned 2.0.0, not be dragged to 2.1.0 by the replaced original's back-edge",
    );
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .unwrap();
    assert_eq!(foo.to_string(), "1.5.0.0-beta1", "got {foo}");
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
    let err = ResolveProvider::build(client, paths, crate::metadata::Repo::from_url("http://x"), &composer_json, true)
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        .filter(|(name, _)| name.as_str() == "acme/iface")
        .collect();
    assert_eq!(iface_entries.len(), 1);
}

#[test]
fn many_versions_of_same_provider_replacing_one_virtual_picks_any() {
    // Magento-style scenario: every patch of magento2-base declares
    // `replace: { components/jquery: "1.11.0" }`. Root pins a
    // specific patch (transitively, via product-community-edition).
    // The virtual selection must not pin magento2-base to the first
    // registered version — that would force every other patch out
    // and produce NoSolution.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let base = p2_body_with_extras(
        "acme/base",
        &[
            ("1.0.0-beta1", json!({}), json!({"replace": {"acme/sub": "1.0.0"}})),
            ("1.0.0", json!({}), json!({"replace": {"acme/sub": "1.0.0"}})),
            ("1.0.0-p1", json!({}), json!({"replace": {"acme/sub": "1.0.0"}})),
            ("1.0.0-p2", json!({}), json!({"replace": {"acme/sub": "1.0.0"}})),
        ],
    );
    // Edition pins a specific (non-beta) base patch.
    let edition = p2_body(
        "acme/edition",
        &[("1.0.0-p2", json!({"acme/base": "1.0.0-p2"}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/base", base).await;
        mount_p2(&server, "acme/edition", edition).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "minimum-stability": "beta",
        "require": {"acme/edition": "1.0.0-p2"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
    provider.pre_fetch_closure().unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let base_version = solution
        .get(&PubGrubPackage::Package("acme/base".into()))
        .expect("base must be in solution");
    assert_eq!(base_version.to_string(), "1.0.0.0-patch2");
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
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
    let err = ResolveProvider::build(client, paths, crate::metadata::Repo::from_url("http://x"), &composer_json, true)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("prefer-stable"), "{msg}");
}

// ===================== Wildcard replace/provide =====================

#[test]
fn wildcard_replace_satisfies_unrelated_range() {
    // The canonical codeception → phpunit-wrapper pattern that
    // unblocked magento2's require-dev: a real package replaces a
    // virtual at `*` (any version), and a consumer requires the
    // virtual in some specific range. The wildcard must absorb the
    // require regardless of the version space.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let monolith = p2_body_with_extras(
        "acme/monolith",
        &[(
            "5.3.5",
            json!({}),
            json!({"replace": {"acme/wrapper": "*"}}),
        )],
    );
    // Consumer requires the wrapper in a range that does NOT
    // include the monolith's own version.
    let consumer = p2_body(
        "acme/consumer",
        &[("2.2.0", json!({"acme/wrapper": "^7.7.1 | ^8.0.3 | ^9.0"}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/monolith", monolith).await;
        mount_p2(&server, "acme/consumer", consumer).await;
        // Deliberately no /p2/acme/wrapper.json — the wildcard
        // replace is the only way to satisfy the require.
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {
            "acme/monolith": "^5.0",
            "acme/consumer": "^2.0",
        },
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
    provider.pre_fetch_closure().unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution
        .get(&PubGrubPackage::Package("acme/monolith".into()))
        .is_some());
    assert!(solution
        .get(&PubGrubPackage::Package("acme/consumer".into()))
        .is_some());
    // The wildcard synthesized a wrapper@<some_version>; it
    // appears in pubgrub's raw solution but is filtered from
    // user-facing output.
    let wrapper_sel = solution
        .get(&PubGrubPackage::Package("acme/wrapper".into()));
    assert!(wrapper_sel.is_some(), "wrapper must be in raw solution");
    // virtual_selections recorded the link back to the provider.
    let selections = provider.virtual_selections.borrow();
    let any_wrapper_entry = selections
        .keys()
        .any(|(n, _)| n.as_str() == "acme/wrapper");
    assert!(any_wrapper_entry, "virtual_selections should record wrapper");
}

#[test]
fn wildcard_replace_does_not_synthesize_without_consumer() {
    // No consumer requires the wrapper. The wildcard should not
    // synthesize a candidate, and the solution should not include
    // the virtual name.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let monolith = p2_body_with_extras(
        "acme/monolith",
        &[(
            "5.0.0",
            json!({}),
            json!({"replace": {"acme/wrapper": "*"}}),
        )],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/monolith", monolith).await;
        (server.uri(), server)
    });

    let composer_json = json!({"require": {"acme/monolith": "^5.0"}});
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
    provider.pre_fetch_closure().unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution
        .get(&PubGrubPackage::Package("acme/monolith".into()))
        .is_some());
    assert!(solution
        .get(&PubGrubPackage::Package("acme/wrapper".into()))
        .is_none());
}

#[test]
fn real_packagist_versions_preferred_over_wildcard() {
    // Both real Packagist versions AND a wildcard provider are
    // available. Real takes precedence (the wildcard fallback only
    // fires when no real or specific-virtual candidate fits).
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let monolith = p2_body_with_extras(
        "acme/monolith",
        &[(
            "5.0.0",
            json!({}),
            json!({"replace": {"acme/wrapper": "*"}}),
        )],
    );
    let wrapper = p2_body("acme/wrapper", &[("9.0.0", json!({}))]);
    let consumer = p2_body(
        "acme/consumer",
        &[("1.0.0", json!({"acme/wrapper": "^9.0"}))],
    );

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/monolith", monolith).await;
        mount_p2(&server, "acme/wrapper", wrapper).await;
        mount_p2(&server, "acme/consumer", consumer).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {
            "acme/monolith": "^5.0",
            "acme/consumer": "^1.0",
        },
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
    provider.pre_fetch_closure().unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    // Real wrapper 9.0.0 picked, not a wildcard synthetic.
    let wrapper_v = solution
        .get(&PubGrubPackage::Package("acme/wrapper".into()))
        .unwrap();
    assert_eq!(wrapper_v.to_string(), "9.0.0.0");
    // The wildcard entry exists in the index but was never
    // synthesized (no fallback was needed), so virtual_selections
    // shouldn't have wrapper@9.0.0 from a wildcard.
}

// ===================== Composer-type repositories =====================

#[test]
fn custom_repo_finds_package_packagist_lacks() {
    // Two repos: custom Composer repo at one wiremock server,
    // public Packagist mock at another. acme/foo only exists on the
    // custom repo. The resolver must find it there.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let foo_body = p2_body("acme/foo", &[("1.0.0", json!({}))]);

    let rt = rt();
    let (custom_uri, packagist_uri, _custom_server, _pkgst_server) = rt.block_on(async {
        let custom = MockServer::start().await;
        mount_p2(&custom, "acme/foo", foo_body).await;
        let pkgst = MockServer::start().await;
        let cu = custom.uri();
        let pu = pkgst.uri();
        (cu, pu, custom, pkgst)
    });

    let composer_json = json!({
        "repositories": [
            {"type": "composer", "url": custom_uri},
        ],
        "require": {"acme/foo": "^1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url(packagist_uri),
        &composer_json,
        true,
    )
    .unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .unwrap();
    assert_eq!(foo.to_string(), "1.0.0.0");
}

#[test]
fn packagist_org_false_disables_public_fallback() {
    // Custom repo has the package; public Packagist also has it but
    // is disabled via `packagist.org: false`. The resolver must
    // succeed via the custom repo and never touch public Packagist.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let foo = p2_body("acme/foo", &[("1.0.0", json!({}))]);

    let rt = rt();
    let (custom_uri, packagist_uri, _custom, _pkgst) = rt.block_on(async {
        let custom = MockServer::start().await;
        mount_p2(&custom, "acme/foo", foo).await;
        let pkgst = MockServer::start().await;
        // Trap mock: any request to public Packagist returns 500.
        // If the resolver ever queries it the test fails.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&pkgst)
            .await;
        (custom.uri(), pkgst.uri(), custom, pkgst)
    });

    let composer_json = json!({
        "repositories": [
            {"type": "composer", "url": custom_uri},
            {"packagist.org": false},
        ],
        "require": {"acme/foo": "^1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url(packagist_uri),
        &composer_json,
        true,
    )
    .unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .is_some());
}

#[test]
fn packagist_org_false_disables_public_fallback_named_object_form() {
    // Same intent as the array-form test above, but with the
    // named/object `repositories` shape Composer also accepts:
    //   "repositories": {
    //       "private": {"type": "composer", "url": "..."},
    //       "packagist.org": false
    //   }
    // bougie used to only parse the array form and silently
    // dropped the entire block, leaving Packagist enabled and the
    // private repo unregistered — opposite of what the user
    // declared.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let foo = p2_body("acme/foo", &[("1.0.0", json!({}))]);

    let rt = rt();
    let (custom_uri, packagist_uri, _custom, _pkgst) = rt.block_on(async {
        let custom = MockServer::start().await;
        mount_p2(&custom, "acme/foo", foo).await;
        let pkgst = MockServer::start().await;
        // Trap: any request to public Packagist fails the test —
        // proves both that the disable took effect AND that the
        // private repo was actually registered (otherwise the
        // resolver would have no source for acme/foo).
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&pkgst)
            .await;
        (custom.uri(), pkgst.uri(), custom, pkgst)
    });

    let composer_json = json!({
        "repositories": {
            "private": {"type": "composer", "url": custom_uri},
            "packagist.org": false,
        },
        "require": {"acme/foo": "^1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url(packagist_uri),
        &composer_json,
        true,
    )
    .unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .is_some());
}

#[test]
fn unknown_repo_type_yields_build_error() {
    // `vcs` / `path` / `package` / `artifact` are ignored silently
    // (follow-up work). A genuinely unrecognized type errors so
    // typos surface fast.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let composer_json = json!({
        "repositories": [{"type": "qwerty", "url": "https://example.test/"}],
        "require": {},
    });
    let client = crate::metadata::build_client().unwrap();
    let err = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url("http://x"),
        &composer_json,
        true,
    )
    .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("qwerty"), "{msg}");
}

#[test]
fn vcs_repo_type_is_ignored_silently() {
    // VCS repositories aren't supported yet but should not break
    // resolution against the public-Packagist fallback for packages
    // that DO live there.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let foo = p2_body("acme/foo", &[("1.0.0", json!({}))]);
    let rt = rt();
    let (packagist_uri, _pkgst) = rt.block_on(async {
        let pkgst = MockServer::start().await;
        mount_p2(&pkgst, "acme/foo", foo).await;
        (pkgst.uri(), pkgst)
    });

    let composer_json = json!({
        "repositories": [
            {"type": "vcs", "url": "https://github.com/acme/foo.git"},
        ],
        "require": {"acme/foo": "^1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url(packagist_uri),
        &composer_json,
        true,
    )
    .unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .is_some());
}

#[test]
fn path_repo_entry_parses_into_path_kind() {
    use crate::metadata::{ReferenceMode, RepoKind};
    let composer_json = json!({
        "repositories": [
            {
                "type": "path",
                "url": "../packages/*",
                "options": {
                    "symlink": false,
                    "relative": true,
                    "reference": "config",
                    "versions": {"acme/local": "2.3-dev"},
                },
            },
            {"packagist.org": false},
        ],
        "require": {"acme/local": "*"},
    });
    let repos = crate::update::read_repositories(
        &composer_json,
        crate::metadata::Repo::from_url("http://unused"),
        &std::collections::HashMap::new(),
    )
    .unwrap();
    assert_eq!(repos.len(), 1, "packagist disabled, one path repo remains");
    let RepoKind::Path(cfg) = &repos[0].kind else {
        panic!("expected a path repo, got {:?}", repos[0].kind);
    };
    assert_eq!(cfg.url, "../packages/*");
    assert_eq!(cfg.symlink, Some(false));
    assert_eq!(cfg.relative, Some(true));
    assert_eq!(cfg.reference, ReferenceMode::Config);
    assert_eq!(cfg.versions.get("acme/local").map(String::as_str), Some("2.3-dev"));
}

#[test]
fn path_repo_defaults_when_options_omitted() {
    use crate::metadata::{ReferenceMode, RepoKind};
    let composer_json = json!({
        "repositories": [{"type": "path", "url": "../pkg"}],
        "require": {},
    });
    let repos = crate::update::read_repositories(
        &composer_json,
        crate::metadata::Repo::from_url("http://unused"),
        &std::collections::HashMap::new(),
    )
    .unwrap();
    let RepoKind::Path(cfg) = &repos[0].kind else {
        panic!("expected a path repo");
    };
    assert_eq!(cfg.symlink, None, "default is Composer's symlink-or-copy");
    assert_eq!(cfg.relative, None, "unset → install-time default (relative) applies");
    assert_eq!(cfg.reference, ReferenceMode::Auto);
    assert!(cfg.versions.is_empty());
}

#[test]
fn path_repo_missing_url_errors() {
    let composer_json = json!({
        "repositories": [{"type": "path"}],
        "require": {},
    });
    let err = crate::update::read_repositories(
        &composer_json,
        crate::metadata::Repo::from_url("http://unused"),
        &std::collections::HashMap::new(),
    )
    .unwrap_err();
    assert!(format!("{err}").contains("missing `url`"), "{err}");
}


/// Write a path-package directory under `root` with the given
/// composer.json contents. Returns the package directory.
fn write_path_package(root: &Path, subdir: &str, composer_json: serde_json::Value) -> std::path::PathBuf {
    let dir = root.join(subdir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("composer.json"),
        serde_json::to_vec_pretty(&composer_json).unwrap(),
    )
    .unwrap();
    dir
}

#[test]
fn path_repo_resolves_and_locks_path_dist() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let paths = paths_in(tmp.path());

    write_path_package(
        &project_root,
        "packages/local",
        json!({
            "name": "acme/local",
            "version": "1.2.3",
            "autoload": {"psr-4": {"Acme\\Local\\": "src/"}},
        }),
    );

    let composer_json = json!({
        "repositories": [
            {"type": "path", "url": "packages/*", "options": {"reference": "none"}},
            {"packagist.org": false},
        ],
        "require": {"acme/local": "*"},
    });

    let client = crate::metadata::build_client().unwrap();
    let mut provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url("http://unused"),
        &composer_json,
        true,
    )
    .unwrap();
    provider.seed_path_candidates(&project_root);
    provider.pre_fetch_closure_silent().unwrap();
    let root = provider.root_version();
    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(
        solution
            .get(&PubGrubPackage::Package("acme/local".into()))
            .is_some(),
        "path package must be in the solution",
    );

    let version = Version::parse("1.2.3").unwrap();
    let locked = provider
        .lock_package_for("acme/local", &version)
        .expect("locked entry present");
    let dist = locked.dist.expect("path package has a dist");
    assert_eq!(dist.kind, "path");
    assert!(dist.shasum.is_none(), "path dists have no shasum");
    assert_eq!(dist.reference, None, "reference: none → null");
    assert_eq!(dist.url, "packages/local");
    assert_eq!(
        locked.autoload.psr_4.get("Acme\\Local\\").and_then(|v| v.as_str()),
        Some("src/"),
    );
}

#[test]
fn path_repo_shadows_packagist() {
    // A path repo declared above Packagist is canonical for its names:
    // even though Packagist serves acme/local, the local copy wins.
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let paths = paths_in(tmp.path());

    write_path_package(
        &project_root,
        "packages/local",
        json!({"name": "acme/local", "version": "9.9.9"}),
    );

    // Packagist would offer a different version; if it were consulted
    // the solver could pick it. The path repo must shadow it.
    let body = p2_body("acme/local", &[("1.0.0", json!({}))]);
    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/local", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "repositories": [
            {"type": "path", "url": "packages/local", "options": {"reference": "none"}},
        ],
        "require": {"acme/local": "*"},
    });

    let client = crate::metadata::build_client().unwrap();
    let mut provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url(uri),
        &composer_json,
        true,
    )
    .unwrap();
    provider.seed_path_candidates(&project_root);
    provider.pre_fetch_closure_silent().unwrap();
    let root = provider.root_version();
    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let picked = solution
        .get(&PubGrubPackage::Package("acme/local".into()))
        .expect("acme/local resolved");
    assert_eq!(
        picked.to_string(),
        "9.9.9.0",
        "the local path version must shadow Packagist's 1.0.0",
    );
}

#[test]
fn path_repo_infers_dev_master_without_version_or_git() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let paths = paths_in(tmp.path());

    // No `version` field and not a git repo → dev-master.
    write_path_package(&project_root, "pkg", json!({"name": "acme/branchy"}));

    let composer_json = json!({
        "repositories": [
            {"type": "path", "url": "pkg", "options": {"reference": "none"}},
            {"packagist.org": false},
        ],
        "require": {"acme/branchy": "dev-master"},
        "minimum-stability": "dev",
    });

    let client = crate::metadata::build_client().unwrap();
    let mut provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url("http://unused"),
        &composer_json,
        true,
    )
    .unwrap();
    provider.seed_path_candidates(&project_root);
    provider.pre_fetch_closure_silent().unwrap();
    let root = provider.root_version();
    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let picked = solution
        .get(&PubGrubPackage::Package("acme/branchy".into()))
        .expect("acme/branchy resolved");
    assert_eq!(picked.to_string(), "dev-master");
}

#[test]
fn path_package_transitive_require_is_crawled_from_packagist() {
    // A path package's runtime `require` on a Packagist package must
    // be fetched and resolved — the prefetch BFS seeds the path
    // package's requires into its frontier.
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let paths = paths_in(tmp.path());

    write_path_package(
        &project_root,
        "packages/local",
        json!({
            "name": "acme/local",
            "version": "1.0.0",
            "require": {"acme/dep": "^2.0"},
        }),
    );

    let body = p2_body("acme/dep", &[("2.1.0", json!({})), ("2.0.0", json!({}))]);
    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/dep", body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "repositories": [
            {"type": "path", "url": "packages/*", "options": {"reference": "none"}},
            {"type": "composer", "url": uri},
            {"packagist.org": false},
        ],
        "require": {"acme/local": "*"},
    });

    let client = crate::metadata::build_client().unwrap();
    let mut provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url("http://unused"),
        &composer_json,
        true,
    )
    .unwrap();
    provider.discover_repos();
    provider.seed_path_candidates(&project_root);
    provider.pre_fetch_closure_silent().unwrap();
    let root = provider.root_version();
    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(
        solution
            .get(&PubGrubPackage::Package("acme/local".into()))
            .is_some(),
        "path package resolved",
    );
    let dep = solution
        .get(&PubGrubPackage::Package("acme/dep".into()))
        .expect("transitive Packagist dep of the path package resolved");
    assert_eq!(dep.to_string(), "2.1.0.0");
}


// ===================== Repository auth =====================

#[test]
fn http_basic_auth_from_config_unlocks_private_repo() {
    use wiremock::matchers::header;
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let body = p2_body("acme/private", &[("1.0.0", json!({}))]);
    // base64("alice:s3cret") = "YWxpY2U6czNjcmV0"
    let expected_header = "Basic YWxpY2U6czNjcmV0";

    let rt = rt();
    let (custom_uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/private.json"))
            .and(header("Authorization", expected_header))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/private.json"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let custom_host = custom_uri
        .strip_prefix("http://")
        .unwrap_or(&custom_uri)
        .split(':')
        .next()
        .unwrap()
        .to_owned();
    let composer_json = json!({
        "repositories": [
            {"type": "composer", "url": custom_uri},
            {"packagist.org": false},
        ],
        "config": {
            "http-basic": {
                custom_host: {"username": "alice", "password": "s3cret"},
            },
        },
        "require": {"acme/private": "^1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let auth = crate::update::read_auth_from_composer_json(&composer_json).unwrap();
    let provider = ResolveProvider::build_with_auth(
        client,
        paths,
        crate::metadata::Repo::from_url("http://unused"),
        &composer_json,
        true,
        auth,
        crate::platform::PlatformEnv::default(),
    )
    .unwrap();
    let root = provider.root_version();
    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution
        .get(&PubGrubPackage::Package("acme/private".into()))
        .is_some());
}

#[test]
fn bearer_auth_from_config_unlocks_private_repo() {
    use wiremock::matchers::header;
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let body = p2_body("acme/gated", &[("2.0.0", json!({}))]);

    let rt = rt();
    let (custom_uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/gated.json"))
            .and(header("Authorization", "Bearer SUPERSECRET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/gated.json"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let custom_host = custom_uri
        .strip_prefix("http://")
        .unwrap_or(&custom_uri)
        .split(':')
        .next()
        .unwrap()
        .to_owned();
    let composer_json = json!({
        "repositories": [
            {"type": "composer", "url": custom_uri.clone()},
            {"packagist.org": false},
        ],
        "config": {
            "bearer": {custom_host: "SUPERSECRET"},
        },
        "require": {"acme/gated": "^2.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let auth = crate::update::read_auth_from_composer_json(&composer_json).unwrap();
    let provider = ResolveProvider::build_with_auth(
        client,
        paths,
        crate::metadata::Repo::from_url("http://unused"),
        &composer_json,
        true,
        auth,
        crate::platform::PlatformEnv::default(),
    )
    .unwrap();
    let root = provider.root_version();
    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution
        .get(&PubGrubPackage::Package("acme/gated".into()))
        .is_some());
}

#[test]
fn auth_json_overrides_composer_json_config() {
    use wiremock::matchers::header;
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let body = p2_body("acme/foo", &[("1.0.0", json!({}))]);

    let rt = rt();
    let (custom_uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        // base64("admin:correct") = "YWRtaW46Y29ycmVjdA=="
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/foo.json"))
            .and(header("Authorization", "Basic YWRtaW46Y29ycmVjdA=="))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/foo.json"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let custom_host = custom_uri
        .strip_prefix("http://")
        .unwrap_or(&custom_uri)
        .split(':')
        .next()
        .unwrap()
        .to_owned();
    // composer.json has the WRONG creds; auth.json has the right.
    let composer_json_value = json!({
        "repositories": [
            {"type": "composer", "url": custom_uri.clone()},
            {"packagist.org": false},
        ],
        "config": {
            "http-basic": {
                custom_host.clone(): {"username": "bob", "password": "wrong"},
            },
        },
        "require": {"acme/foo": "^1.0"},
    });
    let proj = TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        serde_json::to_string(&composer_json_value).unwrap(),
    )
    .unwrap();
    let auth_json = json!({
        "http-basic": {
            custom_host: {"username": "admin", "password": "correct"},
        },
    });
    std::fs::write(
        proj.path().join("auth.json"),
        serde_json::to_string(&auth_json).unwrap(),
    )
    .unwrap();

    let client = crate::metadata::build_client().unwrap();
    let mut auth = crate::update::read_auth_from_composer_json(&composer_json_value).unwrap();
    auth.extend(crate::update::read_auth_json(proj.path()).unwrap());

    let provider = ResolveProvider::build_with_auth(
        client,
        paths,
        crate::metadata::Repo::from_url("http://unused"),
        &composer_json_value,
        true,
        auth,
        crate::platform::PlatformEnv::default(),
    )
    .unwrap();
    let root = provider.root_version();
    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    assert!(solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .is_some());
}

#[test]
fn auth_credentials_debug_redacts_password_and_token() {
    let basic = crate::metadata::AuthCredentials::Basic {
        username: "alice".into(),
        password: "supersecret123".into(),
    };
    let bearer = crate::metadata::AuthCredentials::Bearer {
        token: "tokenZZ".into(),
    };
    let basic_dbg = format!("{basic:?}");
    let bearer_dbg = format!("{bearer:?}");
    assert!(!basic_dbg.contains("supersecret123"), "{basic_dbg}");
    assert!(basic_dbg.contains("alice"), "username should be visible: {basic_dbg}");
    assert!(basic_dbg.contains("redacted"), "{basic_dbg}");
    assert!(!bearer_dbg.contains("tokenZZ"), "{bearer_dbg}");
    assert!(bearer_dbg.contains("redacted"), "{bearer_dbg}");
}

#[test]
fn discover_repos_resolves_via_v1_protocol_end_to_end() {
    // Stand up a v1 Composer repo (packages.json → provider-includes
    // → per-package), `discover_repos` records the protocol on the
    // Repo, and the resolver fetches a package through the v1
    // lookup path. Traps on `/p2/` ensure we don't accidentally fall
    // back to v2 against the v1 server. Mirrors the Magento topology:
    // one provider-include carrying one package, per-package file
    // keyed by version string.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let include_sha = "deadbeef00000000000000000000000000000000000000000000000000000001";
    let pkg_sha = "deadbeef00000000000000000000000000000000000000000000000000000002";

    let packages_json = format!(
        r#"{{
            "packages": [],
            "providers-url": "/p/%package%$%hash%.json",
            "provider-includes": {{
                "p/providers-main$%hash%.json": {{"sha256": "{include_sha}"}}
            }}
        }}"#
    );
    let provider_include = format!(
        r#"{{"providers": {{"acme/foo": {{"sha256": "{pkg_sha}"}}}}}}"#,
    );
    let per_package = r#"{
        "packages": {
            "acme/foo": {
                "1.0.0": {
                    "name": "acme/foo",
                    "version": "1.0.0",
                    "version_normalized": "1.0.0.0",
                    "type": "library",
                    "dist": {
                        "type": "zip",
                        "url": "https://example.test/foo-1.0.0.zip",
                        "shasum": "aaa",
                        "reference": null
                    },
                    "uid": "ignored-v1-internal-id"
                },
                "1.1.0": {
                    "name": "acme/foo",
                    "version": "1.1.0",
                    "version_normalized": "1.1.0.0",
                    "type": "library",
                    "dist": {
                        "type": "zip",
                        "url": "https://example.test/foo-1.1.0.zip",
                        "shasum": "bbb",
                        "reference": null
                    }
                }
            }
        }
    }"#;

    let rt = rt();
    let (v1_uri, packagist_uri, _v1, _pkgst) = rt.block_on(async {
        let v1 = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/packages.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(packages_json))
            .mount(&v1)
            .await;
        Mock::given(method("GET"))
            .and(wm_path(format!("/p/providers-main${include_sha}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_string(provider_include))
            .mount(&v1)
            .await;
        Mock::given(method("GET"))
            .and(wm_path(format!("/p/acme/foo${pkg_sha}.json")))
            .respond_with(ResponseTemplate::new(200).set_body_string(per_package))
            .mount(&v1)
            .await;
        // Trap: any /p2/ request to the v1 server is a bug — the
        // dispatcher must take the v1 path, not the v2 path.
        Mock::given(method("GET"))
            .and(wiremock::matchers::path_regex(r"^/p2/.*"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&v1)
            .await;
        let pkgst = MockServer::start().await;
        (v1.uri(), pkgst.uri(), v1, pkgst)
    });

    let composer_json = json!({
        "repositories": [
            {"type": "composer", "url": v1_uri},
            {"packagist.org": false},
        ],
        "require": {"acme/foo": "^1.0"},
    });
    let client = crate::metadata::build_client().unwrap();
    let mut provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url(packagist_uri),
        &composer_json,
        true,
    )
    .unwrap();
    provider.discover_repos();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .expect("acme/foo should resolve via the v1 lookup path");
    assert_eq!(foo.to_string(), "1.1.0.0", "highest matching version wins");

    // Round-trip the LockPackage back out to confirm dist + version
    // fields parsed cleanly — the v1 entry shape includes `uid` and
    // `reference: null` which both must be tolerated.
    let lp = provider
        .lock_package_for("acme/foo", foo)
        .expect("cached lock package");
    assert_eq!(lp.version, "1.1.0");
    assert_eq!(lp.dist.expect("dist").url, "https://example.test/foo-1.1.0.zip");
}

// --- auth-source tests ---------------------------------------------
//
// The env-driven sources (`read_global_auth_json`, `read_composer_auth_env`)
// can't be unit-tested directly without racing other tests that share
// the same env. We test the pure helpers under them
// (`global_auth_json_candidates`, `parse_composer_auth_env`,
// `read_auth_json_at`) which carry the real logic.

#[test]
fn parse_composer_auth_env_empty_input_yields_empty_map() {
    let out = crate::update::parse_composer_auth_env("").unwrap();
    assert!(out.is_empty());
    let out = crate::update::parse_composer_auth_env("   \n  ").unwrap();
    assert!(out.is_empty());
}

#[test]
fn parse_composer_auth_env_parses_http_basic_and_bearer() {
    let raw = r#"{
        "http-basic": {
            "repo.example.com": {"username": "u", "password": "p"}
        },
        "bearer": {
            "api.example.com": "tok"
        }
    }"#;
    let out = crate::update::parse_composer_auth_env(raw).unwrap();
    assert_eq!(out.len(), 2);
    match out.get("repo.example.com").unwrap() {
        crate::metadata::AuthCredentials::Basic { username, password } => {
            assert_eq!(username, "u");
            assert_eq!(password, "p");
        }
        other => panic!("expected Basic, got {other:?}"),
    }
    match out.get("api.example.com").unwrap() {
        crate::metadata::AuthCredentials::Bearer { token } => {
            assert_eq!(token, "tok");
        }
        other => panic!("expected Bearer, got {other:?}"),
    }
}

#[test]
fn parse_composer_auth_env_rejects_malformed_json_with_named_error() {
    let err = crate::update::parse_composer_auth_env("not json").unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("COMPOSER_AUTH"), "{msg}");
    assert!(msg.contains("not valid JSON"), "{msg}");
}

#[test]
fn parse_composer_auth_env_rejects_non_object_top_level() {
    let err = crate::update::parse_composer_auth_env("[]").unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("COMPOSER_AUTH"), "{msg}");
    assert!(msg.contains("object"), "{msg}");
}

#[test]
fn parse_composer_auth_env_parses_github_oauth_and_gitlab_token() {
    let raw = r#"{
        "github-oauth": {
            "github.com": "ghp_xxxxxxxxxxxx"
        },
        "gitlab-token": {
            "gitlab.com": "glpat-yyyyyyyy"
        },
        "gitlab-oauth": {
            "gitlab.example.com": "oauth-zzzzzzzz"
        }
    }"#;
    let out = crate::update::parse_composer_auth_env(raw).unwrap();
    assert_eq!(out.len(), 3);
    match out.get("github.com").unwrap() {
        crate::metadata::AuthCredentials::GitHubToken { token } => {
            assert_eq!(token, "ghp_xxxxxxxxxxxx");
        }
        other => panic!("expected GitHubToken, got {other:?}"),
    }
    match out.get("gitlab.com").unwrap() {
        crate::metadata::AuthCredentials::GitLabToken { token } => {
            assert_eq!(token, "glpat-yyyyyyyy");
        }
        other => panic!("expected GitLabToken, got {other:?}"),
    }
    match out.get("gitlab.example.com").unwrap() {
        crate::metadata::AuthCredentials::Bearer { token } => {
            assert_eq!(token, "oauth-zzzzzzzz");
        }
        other => panic!("expected Bearer (from gitlab-oauth), got {other:?}"),
    }
}

#[test]
fn github_oauth_header_uses_token_prefix() {
    let creds = crate::metadata::AuthCredentials::GitHubToken {
        token: "ghp_test".to_string(),
    };
    assert_eq!(creds.header_value(), "token ghp_test");
    assert_eq!(creds.header_name(), "authorization");
}

#[test]
fn gitlab_token_uses_private_token_header() {
    let creds = crate::metadata::AuthCredentials::GitLabToken {
        token: "glpat-test".to_string(),
    };
    assert_eq!(creds.header_value(), "glpat-test");
    assert_eq!(creds.header_name(), "private-token");
}

#[test]
fn gitlab_token_object_format_extracts_token() {
    let raw = r#"{
        "gitlab-token": {
            "gitlab.com": {"username": "gitlab-ci-token", "token": "ci-job-token-xxx"}
        }
    }"#;
    let out = crate::update::parse_composer_auth_env(raw).unwrap();
    match out.get("gitlab.com").unwrap() {
        crate::metadata::AuthCredentials::GitLabToken { token } => {
            assert_eq!(token, "ci-job-token-xxx");
        }
        other => panic!("expected GitLabToken, got {other:?}"),
    }
}

#[test]
fn global_auth_json_candidates_honors_composer_home_first() {
    let env = |k: &str| match k {
        "COMPOSER_HOME" => Some("/opt/composer".into()),
        "XDG_CONFIG_HOME" => Some("/xdg".into()),
        "HOME" => Some("/home/u".into()),
        _ => None,
    };
    let got = crate::update::global_auth_json_candidates(env);
    assert_eq!(
        got,
        vec![
            std::path::PathBuf::from("/opt/composer/auth.json"),
            std::path::PathBuf::from("/xdg/composer/auth.json"),
            std::path::PathBuf::from("/home/u/.config/composer/auth.json"),
            std::path::PathBuf::from("/home/u/.composer/auth.json"),
        ],
    );
}

#[test]
fn global_auth_json_candidates_falls_back_to_xdg_then_legacy() {
    // No COMPOSER_HOME, no XDG_CONFIG_HOME — just $HOME. We expect
    // the XDG-default path first, the legacy ~/.composer path second.
    let env = |k: &str| if k == "HOME" { Some("/home/u".into()) } else { None };
    let got = crate::update::global_auth_json_candidates(env);
    assert_eq!(
        got,
        vec![
            std::path::PathBuf::from("/home/u/.config/composer/auth.json"),
            std::path::PathBuf::from("/home/u/.composer/auth.json"),
        ],
    );
}

#[test]
fn global_auth_json_candidates_empty_env_yields_no_candidates() {
    let got = crate::update::global_auth_json_candidates(|_| None);
    assert!(got.is_empty());
}

#[test]
fn bougie_auth_json_candidates_prefers_xdg_then_home() {
    let env = |k: &str| match k {
        "XDG_CONFIG_HOME" => Some("/xdg".into()),
        "HOME" => Some("/home/u".into()),
        _ => None,
    };
    let got = crate::update::bougie_auth_json_candidates(env);
    assert_eq!(
        got,
        vec![
            std::path::PathBuf::from("/xdg/bougie/auth.json"),
            std::path::PathBuf::from("/home/u/.config/bougie/auth.json"),
        ],
    );
    // HOME-only falls back to ~/.config/bougie.
    let env = |k: &str| if k == "HOME" { Some("/home/u".into()) } else { None };
    assert_eq!(
        crate::update::bougie_auth_json_candidates(env),
        vec![std::path::PathBuf::from("/home/u/.config/bougie/auth.json")],
    );
    assert!(crate::update::bougie_auth_json_candidates(|_| None).is_empty());
}

#[test]
fn write_http_basic_at_creates_merges_and_round_trips() {
    // Uses the path-based core so the test never touches the process env
    // (HOME / XDG_CONFIG_HOME), keeping it safe under parallel execution.
    let tmp = TempDir::new().unwrap();
    // A nested dir that doesn't exist yet — the writer must create it.
    let path = tmp.path().join("bougie").join("auth.json");

    crate::update::write_http_basic_at(&path, "hyva-themes.repo.packagist.com", "token", "secret-key")
        .unwrap();
    // A second host merges in rather than clobbering the first.
    crate::update::write_http_basic_at(&path, "other.example", "u2", "p2").unwrap();

    let got = crate::update::read_auth_json_at(&path).unwrap();
    match got.get("hyva-themes.repo.packagist.com").unwrap() {
        crate::metadata::AuthCredentials::Basic { username, password } => {
            assert_eq!(username, "token");
            assert_eq!(password, "secret-key");
        }
        other => panic!("expected Basic, got {other:?}"),
    }
    assert!(got.contains_key("other.example"), "first host must survive the second write");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credential file must be 0600");
    }
}

#[test]
fn read_auth_json_at_returns_empty_for_missing_file() {
    let tmp = TempDir::new().unwrap();
    let out = crate::update::read_auth_json_at(&tmp.path().join("nope.json")).unwrap();
    assert!(out.is_empty());
}

#[test]
fn read_auth_json_at_parses_basic_and_bearer_with_path_in_errors() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("auth.json");
    std::fs::write(
        &path,
        r#"{
            "http-basic": {
                "h": {"username": "u", "password": "p"}
            },
            "bearer": {"b": "t"}
        }"#,
    )
    .unwrap();
    let out = crate::update::read_auth_json_at(&path).unwrap();
    assert_eq!(out.len(), 2);

    // Malformed entry must surface the file path in the error so the
    // user knows which auth.json to fix.
    let bad = tmp.path().join("bad.json");
    std::fs::write(
        &bad,
        r#"{"http-basic": {"h": {"username": 42, "password": "p"}}}"#,
    )
    .unwrap();
    let err = crate::update::read_auth_json_at(&bad).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("bad.json"), "{msg}");
    assert!(msg.contains("http-basic.h.username"), "{msg}");
}

#[test]
fn merge_auth_sources_follows_composer_precedence() {
    // Pins Composer's precedence order (see `Composer\Factory.php`
    // lines 209-219 + 328-344). Lowest → highest:
    //   1. global auth.json
    //   2. composer.json `config`
    //   3. project auth.json
    //   4. COMPOSER_AUTH env
    // We supply the same host in every source with a distinct
    // password marker, then assert the env-var marker wins.
    use crate::metadata::AuthCredentials;
    fn basic(p: &str) -> AuthCredentials {
        AuthCredentials::Basic { username: "u".into(), password: p.into() }
    }
    let host = "example.com";

    // env wins over everything.
    let out = crate::update::merge_auth_sources(
        [(host.into(), basic("global"))].into_iter().collect(),
        [(host.into(), basic("composer.json"))].into_iter().collect(),
        [(host.into(), basic("project"))].into_iter().collect(),
        [(host.into(), basic("env"))].into_iter().collect(),
    );
    match out.get(host).unwrap() {
        AuthCredentials::Basic { password, .. } => assert_eq!(password, "env"),
        _ => unreachable!(),
    }

    // Without env, project wins over composer.json wins over global.
    let out = crate::update::merge_auth_sources(
        [(host.into(), basic("global"))].into_iter().collect(),
        [(host.into(), basic("composer.json"))].into_iter().collect(),
        [(host.into(), basic("project"))].into_iter().collect(),
        HashMap::new(),
    );
    match out.get(host).unwrap() {
        AuthCredentials::Basic { password, .. } => assert_eq!(password, "project"),
        _ => unreachable!(),
    }

    // Just global + composer.json: composer.json wins (this is the
    // case the previous implementation got backwards).
    let out = crate::update::merge_auth_sources(
        [(host.into(), basic("global"))].into_iter().collect(),
        [(host.into(), basic("composer.json"))].into_iter().collect(),
        HashMap::new(),
        HashMap::new(),
    );
    match out.get(host).unwrap() {
        AuthCredentials::Basic { password, .. } => assert_eq!(password, "composer.json"),
        _ => unreachable!(),
    }
}

#[test]
fn read_all_auth_merges_composer_json_and_project_auth_json() {
    // composer.json supplies one entry, project auth.json supplies a
    // different one — the merged map carries both. Project auth.json
    // overrides composer.json on conflicts (regression guard).
    let proj = TempDir::new().unwrap();
    let composer_json_value = serde_json::json!({
        "config": {
            "http-basic": {
                "from-composer.json": {"username": "u1", "password": "p1"},
                "shared": {"username": "loser", "password": "loser-p"},
            },
        },
    });
    let auth_json = serde_json::json!({
        "http-basic": {
            "from-auth.json": {"username": "u2", "password": "p2"},
            "shared": {"username": "winner", "password": "winner-p"},
        },
    });
    std::fs::write(
        proj.path().join("auth.json"),
        serde_json::to_string(&auth_json).unwrap(),
    )
    .unwrap();

    let out = crate::update::read_all_auth(&composer_json_value, proj.path()).unwrap();
    assert!(out.contains_key("from-composer.json"));
    assert!(out.contains_key("from-auth.json"));
    match out.get("shared").unwrap() {
        crate::metadata::AuthCredentials::Basic { username, .. } => {
            assert_eq!(username, "winner", "project auth.json must win over composer.json");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn analyze_resolution_problems_finds_multiple_missing_versions() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let foo_body = p2_body("acme/foo", &[("1.0.0", json!({})), ("2.0.0", json!({}))]);
    let bar_body = p2_body("acme/bar", &[("3.0.0", json!({})), ("4.0.0", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", foo_body).await;
        mount_p2(&server, "acme/bar", bar_body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {
            "acme/foo": "1.5.0",
            "acme/bar": "3.5.0",
        },
    });
    let client = crate::metadata::build_client().unwrap();
    let provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url(uri),
        &composer_json,
        true,
    )
    .unwrap();
    provider.pre_fetch_closure().unwrap();
    let root = provider.root_version();

    let err = resolve(&provider, PubGrubPackage::Root, root).unwrap_err();
    assert!(matches!(err, pubgrub::PubGrubError::NoSolution(_)));

    let report = provider.analyze_resolution_problems();
    let report = report.expect("should find problems");
    assert!(report.contains("Problem 1"), "report:\n{report}");
    assert!(report.contains("Problem 2"), "report:\n{report}");
    assert!(report.contains("acme/foo"), "report:\n{report}");
    assert!(report.contains("acme/bar"), "report:\n{report}");
}

#[test]
fn analyze_resolution_problems_detects_transitive_conflict() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let foo_body = p2_body("acme/foo", &[("1.0.0", json!({"acme/bar": "^2.0"}))]);
    let bar_body = p2_body("acme/bar", &[("1.0.0", json!({})), ("2.0.0", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", foo_body).await;
        mount_p2(&server, "acme/bar", bar_body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {
            "acme/foo": "^1.0",
            "acme/bar": "^1.0",
        },
    });
    let client = crate::metadata::build_client().unwrap();
    let provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url(uri),
        &composer_json,
        true,
    )
    .unwrap();
    provider.pre_fetch_closure().unwrap();
    let root = provider.root_version();

    let err = resolve(&provider, PubGrubPackage::Root, root).unwrap_err();
    assert!(matches!(err, pubgrub::PubGrubError::NoSolution(_)));

    let report = provider.analyze_resolution_problems();
    let report = report.expect("should find transitive conflict");
    assert!(report.contains("Problem 1"), "report:\n{report}");
    assert!(report.contains("acme/foo"), "report:\n{report}");
    assert!(report.contains("acme/bar"), "report:\n{report}");
    assert!(report.contains("conflicts"), "report:\n{report}");
}

#[test]
fn repo_host_extracts_origin_for_per_host_throttle() {
    // Same host across different paths/ports → same limiter key.
    assert_eq!(super::repo_host("https://repo.mage-os.org"), "repo.mage-os.org");
    assert_eq!(
        super::repo_host("https://repo.mage-os.org/p2/foo/bar.json"),
        "repo.mage-os.org"
    );
    assert_eq!(super::repo_host("https://repo.packagist.org"), "repo.packagist.org");
    // Unparseable URL falls back to the raw string (still a stable key).
    assert_eq!(super::repo_host("not a url"), "not a url");
}

#[test]
fn read_root_replaces_collects_only_wildcards() {
    let composer_json = json!({
        "require": {"acme/foo": "^1.0"},
        "replace": {
            "acme/disabled": "*",      // honored
            "acme/other": "*",         // honored
            "acme/pinned": "1.2.3",    // version replace — not honored (yet)
            "ext-intl": "*",           // platform — ignored
        },
    });
    let names = super::read_root_replaces(&composer_json);
    assert!(names.contains(&PackageName::from("acme/disabled")));
    assert!(names.contains(&PackageName::from("acme/other")));
    assert!(!names.contains(&PackageName::from("acme/pinned")));
    assert!(!names.contains(&PackageName::from("ext-intl")));
    assert_eq!(names.len(), 2);
}

#[test]
fn root_wildcard_replace_excludes_package_and_its_exclusive_deps() {
    // `acme/meta` (a metapackage-style root require) pulls in `acme/mod`
    // and `acme/keep`. `acme/mod` has its own exclusive dep `acme/only`.
    // The root `replace`s `acme/mod` with `*`, so neither `acme/mod` nor
    // `acme/only` may be installed — mirrors Mage-OS disabling a module.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let meta_body = p2_body(
        "acme/meta",
        &[("1.0.0", json!({"acme/mod": "^2.0", "acme/keep": "^1.0"}))],
    );
    let mod_body = p2_body("acme/mod", &[("2.5.0", json!({"acme/only": "^1.0"}))]);
    let keep_body = p2_body("acme/keep", &[("1.0.0", json!({}))]);
    let only_body = p2_body("acme/only", &[("1.0.0", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/meta", meta_body).await;
        // `acme/mod` and `acme/only` ARE mounted: if the fix regressed,
        // they would resolve and show up — so the absence assertions
        // below are meaningful, not just "metadata missing".
        mount_p2(&server, "acme/mod", mod_body).await;
        mount_p2(&server, "acme/keep", keep_body).await;
        mount_p2(&server, "acme/only", only_body).await;
        (server.uri(), server)
    });

    let composer_json = json!({
        "require": {"acme/meta": "^1.0"},
        "replace": {"acme/mod": "*"},
    });
    let client = crate::metadata::build_client().unwrap();
    let provider =
        ResolveProvider::build(client, paths, crate::metadata::Repo::from_url(uri), &composer_json, true).unwrap();
    let root = provider.root_version();

    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();

    // Kept dependency resolves normally.
    assert!(
        solution.get(&PubGrubPackage::Package("acme/keep".into())).is_some(),
        "acme/keep should resolve",
    );
    // The root-replaced package is gone…
    assert!(
        solution.get(&PubGrubPackage::Package("acme/mod".into())).is_none(),
        "acme/mod is replaced by the root and must not be in the solution",
    );
    // …and so is its exclusive transitive dep (proves the edge was
    // dropped, not just the package filtered post-solve).
    assert!(
        solution.get(&PubGrubPackage::Package("acme/only".into())).is_none(),
        "acme/only is only needed by the replaced acme/mod and must not resolve",
    );
    // Neither was even fetched: only acme/meta + acme/keep.
    assert_eq!(provider.cache_size(), 2, "only acme/meta + acme/keep fetched");
}

// ---------------------------------------------------------------------
// Partial update (`composer update <pkg>...`) — pinning out-of-scope
// packages to their locked version via `set_locked_pins`.
// ---------------------------------------------------------------------

/// Build a minimal `LockPackage` from name + version (+ optional
/// requires) by deserializing — most fields default, so this stays terse.
fn lock_pkg(name: &str, version: &str, require: serde_json::Value) -> LockPackage {
    serde_json::from_value(json!({
        "name": name,
        "version": version,
        "version_normalized": format!("{version}.0"),
        "require": require,
    }))
    .unwrap()
}

/// Build a `Lock` holding just these `packages` (every other field
/// defaults — `Lock` has `#[serde(default)]` on all of them).
fn lock_with(packages: Vec<LockPackage>) -> Lock {
    let mut lock: Lock = serde_json::from_value(json!({})).unwrap();
    lock.packages = packages;
    lock
}

#[test]
fn partial_update_pins_out_of_scope_package() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    // Both packages have a newer release available.
    let foo_body = p2_body("acme/foo", &[("1.5.0", json!({})), ("1.0.0", json!({}))]);
    let bar_body = p2_body("acme/bar", &[("2.5.0", json!({})), ("2.1.0", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", foo_body).await;
        mount_p2(&server, "acme/bar", bar_body).await;
        (server.uri(), server)
    });

    let composer_json = json!({"require": {"acme/foo": "^1.0", "acme/bar": "^2.0"}});
    let client = crate::metadata::build_client().unwrap();
    let mut provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url(uri),
        &composer_json,
        true,
    )
    .unwrap();

    // We're updating only acme/foo; acme/bar stays at its locked 2.1.0.
    let partial = PartialUpdate {
        names: vec!["acme/foo".into()],
        with_dependencies: false,
        with_all_dependencies: false,
        root_requires: vec!["acme/foo".into(), "acme/bar".into()],
        lock: lock_with(vec![
            lock_pkg("acme/foo", "1.0.0", json!({})),
            lock_pkg("acme/bar", "2.1.0", json!({})),
        ]),
    };
    provider.set_locked_pins(partial.locked_pins());

    let root = provider.root_version();
    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();

    let foo = solution
        .get(&PubGrubPackage::Package("acme/foo".into()))
        .expect("acme/foo resolves");
    let bar = solution
        .get(&PubGrubPackage::Package("acme/bar".into()))
        .expect("acme/bar resolves");
    // Named package floats up; the pinned one stays put.
    assert_eq!(foo.to_string(), "1.5.0.0", "named package updates");
    assert_eq!(bar.to_string(), "2.1.0.0", "out-of-scope package stays pinned");
}

#[test]
fn partial_update_with_dependencies_floats_transitive() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let foo_body = p2_body(
        "acme/foo",
        &[("1.5.0", json!({"acme/bar": "^2.0"})), ("1.0.0", json!({"acme/bar": "^2.0"}))],
    );
    let bar_body = p2_body("acme/bar", &[("2.5.0", json!({})), ("2.1.0", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", foo_body).await;
        mount_p2(&server, "acme/bar", bar_body).await;
        (server.uri(), server)
    });

    let composer_json = json!({"require": {"acme/foo": "^1.0"}});
    let client = crate::metadata::build_client().unwrap();
    let mut provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url(uri),
        &composer_json,
        true,
    )
    .unwrap();

    // Updating acme/foo --with-dependencies lets its dep acme/bar float too.
    let partial = PartialUpdate {
        names: vec!["acme/foo".into()],
        with_dependencies: true,
        with_all_dependencies: false,
        // acme/bar is only a transitive dep (not a root require), so `-w`
        // lets it float.
        root_requires: vec!["acme/foo".into()],
        lock: lock_with(vec![
            lock_pkg("acme/foo", "1.0.0", json!({"acme/bar": "^2.0"})),
            lock_pkg("acme/bar", "2.1.0", json!({})),
        ]),
    };
    assert!(
        partial.locked_pins().is_empty(),
        "acme/bar is in scope via --with-dependencies, so nothing is pinned",
    );
    provider.set_locked_pins(partial.locked_pins());

    let root = provider.root_version();
    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let bar = solution
        .get(&PubGrubPackage::Package("acme/bar".into()))
        .expect("acme/bar resolves");
    assert_eq!(bar.to_string(), "2.5.0.0", "transitive dep floats with -w");
}

#[test]
fn partial_update_pin_falls_back_when_locked_version_gone() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    // Locked at 2.0.0, but the registry no longer lists it — only 2.5.0.
    let foo_body = p2_body("acme/foo", &[("1.0.0", json!({}))]);
    let bar_body = p2_body("acme/bar", &[("2.5.0", json!({}))]);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        mount_p2(&server, "acme/foo", foo_body).await;
        mount_p2(&server, "acme/bar", bar_body).await;
        (server.uri(), server)
    });

    let composer_json = json!({"require": {"acme/foo": "^1.0", "acme/bar": "^2.0"}});
    let client = crate::metadata::build_client().unwrap();
    let mut provider = ResolveProvider::build(
        client,
        paths,
        crate::metadata::Repo::from_url(uri),
        &composer_json,
        true,
    )
    .unwrap();

    let partial = PartialUpdate {
        names: vec!["acme/foo".into()],
        with_dependencies: false,
        with_all_dependencies: false,
        root_requires: vec!["acme/foo".into(), "acme/bar".into()],
        lock: lock_with(vec![
            lock_pkg("acme/foo", "1.0.0", json!({})),
            lock_pkg("acme/bar", "2.0.0", json!({})),
        ]),
    };
    provider.set_locked_pins(partial.locked_pins());

    let root = provider.root_version();
    let solution = resolve(&provider, PubGrubPackage::Root, root).unwrap();
    let bar = solution
        .get(&PubGrubPackage::Package("acme/bar".into()))
        .expect("acme/bar resolves");
    // The pinned 2.0.0 is unavailable, so it floats rather than dead-ending.
    assert_eq!(bar.to_string(), "2.5.0.0", "missing pin falls back to a free choice");
}

/// Pure check of the `-w` vs `-W` scope rule via the resulting pin set.
///
/// Graph: foo (named, root require) → bar (root require) → baz (not a
/// root require).
/// - `-w` leaves root requires pinned and doesn't recurse through them,
///   so bar stays pinned and baz (only reachable via bar) stays pinned
///   too — only foo floats.
/// - `-W` floats the whole closure, so nothing is pinned.
#[test]
fn with_dependencies_vs_with_all_dependencies_scope() {
    let lock = lock_with(vec![
        lock_pkg("acme/foo", "1.0.0", json!({"acme/bar": "^2.0"})),
        lock_pkg("acme/bar", "2.0.0", json!({"acme/baz": "^3.0"})),
        lock_pkg("acme/baz", "3.0.0", json!({})),
    ]);
    let root_requires = vec!["acme/foo".to_string(), "acme/bar".to_string()];

    // `-w`: bar is a root require → pinned, and baz (only via bar) → pinned.
    let w = PartialUpdate {
        names: vec!["acme/foo".into()],
        with_dependencies: true,
        with_all_dependencies: false,
        root_requires: root_requires.clone(),
        lock: lock.clone(),
    };
    let mut pinned: Vec<String> = w
        .locked_pins()
        .keys()
        .map(std::string::ToString::to_string)
        .collect();
    pinned.sort();
    assert_eq!(pinned, vec!["acme/bar", "acme/baz"], "-w keeps root requires pinned");

    // `-W`: full closure floats → nothing pinned.
    let w_all = PartialUpdate {
        names: vec!["acme/foo".into()],
        with_dependencies: false,
        with_all_dependencies: true,
        root_requires,
        lock,
    };
    assert!(
        w_all.locked_pins().is_empty(),
        "-W floats the whole closure, leaving nothing pinned",
    );
}
