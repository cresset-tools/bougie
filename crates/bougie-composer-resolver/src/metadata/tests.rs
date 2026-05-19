//! Wiremock-driven tests for the Packagist v2 metadata fetcher.

use super::*;
use bougie_paths::Paths;
use std::path::Path;
use tempfile::TempDir;
use wiremock::matchers::{header, method, path as wm_path};
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

/// Minimal `/p2/` body: one package, one fully-expanded version.
fn fixture_body(name: &str, version: &str) -> String {
    format!(
        r#"{{
            "packages": {{
                "{name}": [
                    {{
                        "name": "{name}",
                        "version": "{version}",
                        "version_normalized": "{version}.0",
                        "type": "library",
                        "dist": {{"type":"zip","url":"https://e/a.zip","shasum":"aa"}}
                    }}
                ]
            }}
        }}"#
    )
}

#[test]
fn fetches_and_parses_fully_expanded_response() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let body = fixture_body("acme/foo", "3.0.0");

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/foo.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", "\"abc\"")
                    .set_body_string(body.clone()),
            )
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let client = build_client().unwrap();
    let md = fetch_package_metadata(&client, &paths, &uri, "acme/foo", Variant::Stable).unwrap();
    assert_eq!(md.packages["acme/foo"][0].version, "3.0.0");
}

#[test]
fn fetches_minified_response_and_expands_inheritance() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let body = r#"{
        "minified": "composer/2.0",
        "packages": {
            "acme/bar": [
                {
                    "name":"acme/bar",
                    "version":"3.0.0",
                    "version_normalized":"3.0.0.0",
                    "type":"library",
                    "dist":{"type":"zip","url":"https://e/a","shasum":"a"},
                    "require":{"php":">=8.1"}
                },
                {
                    "version":"2.0.0",
                    "version_normalized":"2.0.0.0",
                    "dist":{"type":"zip","url":"https://e/b","shasum":"b"}
                }
            ]
        }
    }"#;

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/bar.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let client = build_client().unwrap();
    let md = fetch_package_metadata(&client, &paths, &uri, "acme/bar", Variant::Stable).unwrap();
    let v = &md.packages["acme/bar"];
    assert_eq!(v.len(), 2);
    // Second entry inherits `name`, `type`, and `require` from first.
    assert_eq!(v[1].name, "acme/bar");
    assert_eq!(v[1].require.get("php").unwrap(), ">=8.1");
}

#[test]
fn writes_etag_sidecar_after_200() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let body = fixture_body("acme/sidecar", "1.0.0");

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/sidecar.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", "\"xyz-tag\"")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let client = build_client().unwrap();
    fetch_package_metadata(&client, &paths, &uri, "acme/sidecar", Variant::Stable).unwrap();

    let (_json, etag) = cache_paths(&paths, "acme/sidecar", Variant::Stable);
    let on_disk = std::fs::read_to_string(&etag).unwrap();
    assert_eq!(on_disk, "\"xyz-tag\"");
}

#[test]
fn conditional_get_returns_cached_body_on_304() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let etag = "\"baseline-tag\"";
    let body = fixture_body("acme/condget", "2.5.0");

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        // Order matters: wiremock returns the first matching mock, so
        // register the conditional (header-matched) one first. A
        // request *without* `If-None-Match` falls through to the
        // unconditional 200 mock.
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/condget.json"))
            .and(header("If-None-Match", etag))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/condget.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("ETag", etag)
                    .set_body_string(body.clone()),
            )
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let client = build_client().unwrap();
    let first =
        fetch_package_metadata(&client, &paths, &uri, "acme/condget", Variant::Stable).unwrap();
    assert_eq!(first.packages["acme/condget"][0].version, "2.5.0");
    // Second call: server returns 304, fetcher reads the cached body
    // and re-parses it.
    let second =
        fetch_package_metadata(&client, &paths, &uri, "acme/condget", Variant::Stable).unwrap();
    assert_eq!(second.packages["acme/condget"][0].version, "2.5.0");
}

#[test]
fn dev_variant_hits_tilde_dev_url() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let body = fixture_body("acme/branch", "dev-main");

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/branch~dev.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let client = build_client().unwrap();
    let md =
        fetch_package_metadata(&client, &paths, &uri, "acme/branch", Variant::Dev).unwrap();
    assert_eq!(md.packages["acme/branch"][0].version, "dev-main");

    // Cache paths reflect the ~dev suffix.
    let (json, etag) = cache_paths(&paths, "acme/branch", Variant::Dev);
    assert!(json.to_string_lossy().ends_with("acme/branch~dev.json"));
    assert!(etag.to_string_lossy().ends_with("acme/branch~dev.etag"));
}

#[test]
fn server_error_is_surfaced() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/missing.json"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let client = build_client().unwrap();
    let err = fetch_package_metadata(&client, &paths, &uri, "acme/missing", Variant::Stable)
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("404"), "{msg}");
}
