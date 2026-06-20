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
    let md = fetch_package_metadata(&client, &paths, &Repo::from_url(&uri), "acme/foo", Variant::Stable).unwrap();
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
    let md = fetch_package_metadata(&client, &paths, &Repo::from_url(&uri), "acme/bar", Variant::Stable).unwrap();
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
    fetch_package_metadata(&client, &paths, &Repo::from_url(&uri), "acme/sidecar", Variant::Stable).unwrap();

    let (_json, etag) = cache_paths(&paths, &Repo::from_url(&uri), "acme/sidecar", Variant::Stable);
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
        fetch_package_metadata(&client, &paths, &Repo::from_url(&uri), "acme/condget", Variant::Stable).unwrap();
    assert_eq!(first.packages["acme/condget"][0].version, "2.5.0");
    // Second call: server returns 304, fetcher reads the cached body
    // and re-parses it.
    let second =
        fetch_package_metadata(&client, &paths, &Repo::from_url(&uri), "acme/condget", Variant::Stable).unwrap();
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
        fetch_package_metadata(&client, &paths, &Repo::from_url(&uri), "acme/branch", Variant::Dev).unwrap();
    assert_eq!(md.packages["acme/branch"][0].version, "dev-main");

    // Cache paths reflect the ~dev suffix.
    let (json, etag) = cache_paths(&paths, &Repo::from_url(&uri), "acme/branch", Variant::Dev);
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
    let err = fetch_package_metadata(&client, &paths, &Repo::from_url(&uri), "acme/missing", Variant::Stable)
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("404"), "{msg}");
}

#[test]
fn non_json_2xx_body_is_treated_as_miss_not_parse_error() {
    // Reproduces the Composer v1 repo case (repo.magento.com):
    // `/p2/<name>.json` returns 200 `text/html` (after reqwest
    // transparently follows a 302 to a marketing landing page).
    // The optional fetcher must treat this as a repo miss — not
    // crash with "expected value at line 1 column 1" — and must
    // *not* write the HTML body to the on-disk metadata cache.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/notmine.json"))
            .respond_with(
                // `set_body_raw` takes an explicit MIME — wiremock's
                // `set_body_string` hardcodes `text/plain` regardless
                // of any later `insert_header`.
                ResponseTemplate::new(200).set_body_raw(
                    "<html><body>hi</body></html>".as_bytes().to_vec(),
                    "text/html",
                ),
            )
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let client = build_client().unwrap();
    let repo = Repo::from_url(&uri);
    let got = fetch_package_metadata_optional(
        &client, &paths, &repo, "acme/notmine", Variant::Stable,
    )
    .unwrap();
    assert!(got.is_none(), "non-JSON body should be Ok(None), got {got:?}");

    let (json_path, _etag_path) = cache_paths(&paths, &repo, "acme/notmine", Variant::Stable);
    assert!(
        !json_path.exists(),
        "HTML body must not be cached at {}",
        json_path.display(),
    );
}

#[test]
fn probe_protocol_classifies_v2_repo_by_metadata_url() {
    // Composer v2 servers (Packagist, satis with v2 build) advertise
    // a top-level `metadata-url` string. Presence of that field is
    // the canonical signal that `/p2/<name>.json` lookups are valid.
    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/packages.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{
                    "metadata-url": "/p2/%package%.json",
                    "providers-api": "https://x/y/%package%",
                    "search": "https://x/search.json?q=%query%"
                }"#,
            ))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let client = build_client().unwrap();
    let protocol = probe_protocol(&client, &Repo::from_url(&uri)).unwrap();
    assert!(matches!(protocol, RepoProtocol::V2), "got {protocol:?}");
}

#[test]
fn probe_protocol_classifies_v1_repo_when_metadata_url_missing() {
    // Composer v1 servers (repo.magento.com, older satis builds)
    // ship `provider-includes` / `providers-url` and have no
    // `metadata-url`. The probe must capture the providers-url
    // template + each include's path/sha256 so the v1 fetcher can
    // drive the lookup.
    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/packages.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{
                    "packages": [],
                    "provider-includes": {
                        "p/providers-2024$%hash%.json": {"sha256": "aa"}
                    },
                    "providers-url": "/p/%package%$%hash%.json"
                }"#,
            ))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let client = build_client().unwrap();
    let protocol = probe_protocol(&client, &Repo::from_url(&uri)).unwrap();
    let RepoProtocol::V1(discovery) = protocol else {
        panic!("expected V1, got {protocol:?}");
    };
    assert_eq!(discovery.providers_url, "/p/%package%$%hash%.json");
    assert_eq!(discovery.provider_includes.len(), 1);
    assert_eq!(
        discovery.provider_includes[0].path_template,
        "p/providers-2024$%hash%.json",
    );
    assert_eq!(discovery.provider_includes[0].sha256, "aa");
}

#[test]
fn probe_protocol_propagates_http_failure() {
    // 401 / 5xx from packages.json must surface as Err so the
    // orchestrator's "probe failed → keep the repo" branch fires
    // instead of mis-classifying a credentialed v2 repo as v1.
    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/packages.json"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let client = build_client().unwrap();
    let err = probe_protocol(&client, &Repo::from_url(&uri)).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("401"), "{msg}");
}

#[test]
fn outbound_request_carries_bougie_user_agent() {
    // The wire-test for the shared UA: a mock that only matches when
    // the request's `User-Agent` starts with `bougie/`. If the client
    // forgot to set it (e.g. somebody re-introduces a bare
    // `Client::builder().build()`), the matcher misses and the
    // fetcher gets a 404, surfacing as a hard error.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let body = fixture_body("acme/ua", "1.0.0");

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/p2/acme/ua.json"))
            .and(wiremock::matchers::header_regex(
                "User-Agent",
                r"bougie/.*\(\+https://github\.com/cresset-tools/bougie\)$",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let client = build_client().unwrap();
    let md =
        fetch_package_metadata(&client, &paths, &Repo::from_url(&uri), "acme/ua", Variant::Stable).unwrap();
    assert_eq!(md.packages["acme/ua"][0].version, "1.0.0");
}

#[test]
fn auth_origin_keeps_explicit_port() {
    // Composer keys credentials by origin (host + explicit port). A
    // mirror on a non-default port — the classic local-satis shape —
    // must key by `host:port`, or its `auth.json` entry never matches.
    assert_eq!(
        auth_origin("http://127.0.0.1:8080/jelle2/klant/p2/acme/foo.json"),
        "127.0.0.1:8080",
    );
    assert_eq!(auth_origin("http://127.0.0.1:8080"), "127.0.0.1:8080");
    // No explicit port → bare host, exactly like Composer's getOrigin.
    assert_eq!(
        auth_origin("https://repo.packagist.org/p2/acme/foo.json"),
        "repo.packagist.org",
    );
    assert_eq!(auth_origin("https://repo.example.com"), "repo.example.com");
}

#[test]
fn auth_origin_differs_from_cache_namespace_on_port() {
    // The two helpers diverge precisely on the port: the cache
    // namespace strips it (filesystem-safe dir name), the auth origin
    // keeps it (credential lookup key). Regression guard for the 401
    // that prompted this split.
    let url = "http://127.0.0.1:8080/satis";
    assert_eq!(extract_cache_namespace(url), "127.0.0.1");
    assert_eq!(auth_origin(url), "127.0.0.1:8080");
}
