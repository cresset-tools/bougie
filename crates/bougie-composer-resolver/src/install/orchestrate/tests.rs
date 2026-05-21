//! Preflight + content-hash unit tests. None hit the network — every
//! reject path runs before the downloader is invoked.

use super::*;
use bougie_paths::Paths;
use std::path::Path;
use tempfile::TempDir;

fn paths_in(tmp: &Path) -> Paths {
    let home = tmp.join("home");
    let cache = tmp.join("cache");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    Paths::new(home, cache)
}

/// Write `composer.json` + `composer.lock` to `dir` with the given
/// bytes. Returns `dir` for chaining.
fn write_project(dir: &Path, json: &str, lock: &str) {
    std::fs::write(dir.join("composer.json"), json).unwrap();
    std::fs::write(dir.join("composer.lock"), lock).unwrap();
}

/// Compute a valid content-hash for a composer.json body so the
/// fixture lockfile carries the right value and verify_content_hash
/// passes (letting preflight be the part the test actually exercises).
fn hash_for(composer_json: &str) -> String {
    bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap()
}

const MINIMAL_COMPOSER_JSON: &str = r#"{
    "name": "acme/test",
    "require": {}
}"#;

#[test]
fn content_hash_mismatch_errors_with_helpful_message() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    let lock = r#"{
        "content-hash": "0000000000000000000000000000000a",
        "packages": [],
        "packages-dev": []
    }"#;
    write_project(&proj, MINIMAL_COMPOSER_JSON, lock);

    let err = install_from_lock(&paths, &proj, InstallOptions::default())
        .expect_err("must error on hash mismatch");
    let msg = format!("{err:#}");
    assert!(msg.contains("out of sync"), "{msg}");
    assert!(msg.contains("composer update"), "{msg}");
}

#[test]
fn missing_composer_lock_errors_with_helpful_message() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("composer.json"), MINIMAL_COMPOSER_JSON).unwrap();

    let err = install_from_lock(&paths, &proj, InstallOptions::default())
        .expect_err("must error when lock is missing");
    let msg = format!("{err:#}");
    assert!(msg.contains("composer.lock"), "{msg}");
    assert!(msg.contains("composer update"), "must suggest fix: {msg}");
}

#[test]
fn missing_composer_json_errors_with_helpful_message() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();

    let err = install_from_lock(&paths, &proj, InstallOptions::default())
        .expect_err("must error when composer.json is missing");
    let msg = format!("{err:#}");
    assert!(msg.contains("not a Composer project"), "{msg}");
}

#[test]
fn composer_plugin_package_is_rejected_in_preflight() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    let hash = hash_for(MINIMAL_COMPOSER_JSON);
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [
                {{
                    "name": "acme/plugin",
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
    write_project(&proj, MINIMAL_COMPOSER_JSON, &lock);

    let err = install_from_lock(&paths, &proj, InstallOptions::default())
        .expect_err("must reject composer plugin");
    let msg = format!("{err:#}");
    assert!(msg.contains("acme/plugin"), "{msg}");
    assert!(msg.contains("Composer plugin"), "{msg}");
    assert!(msg.contains("bougie run -- composer install"), "{msg}");
}

#[test]
fn source_only_package_is_rejected_in_preflight() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    let hash = hash_for(MINIMAL_COMPOSER_JSON);
    // No `dist` block — only `source` (git).
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [
                {{
                    "name": "acme/sourceonly",
                    "version": "1.0.0",
                    "source": {{
                        "type": "git",
                        "url": "https://example/acme/sourceonly.git",
                        "reference": "abc"
                    }}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project(&proj, MINIMAL_COMPOSER_JSON, &lock);

    let err = install_from_lock(&paths, &proj, InstallOptions::default())
        .expect_err("must reject source-only package");
    let msg = format!("{err:#}");
    assert!(msg.contains("acme/sourceonly"), "{msg}");
    assert!(msg.contains("source-only"), "{msg}");
}

#[test]
fn tar_dist_kind_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    let hash = hash_for(MINIMAL_COMPOSER_JSON);
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [
                {{
                    "name": "acme/tar",
                    "version": "1.0.0",
                    "dist": {{
                        "type": "tar",
                        "url": "https://example/t.tar.gz",
                        "shasum": "1111111111111111111111111111111111111111"
                    }}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project(&proj, MINIMAL_COMPOSER_JSON, &lock);

    let err = install_from_lock(&paths, &proj, InstallOptions::default())
        .expect_err("must reject tar dist");
    let msg = format!("{err:#}");
    assert!(msg.contains("acme/tar"), "{msg}");
    assert!(msg.contains("`tar`"), "{msg}");
    assert!(msg.contains("zip dists"), "{msg}");
}

#[test]
fn composer_json_with_scripts_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    let composer_json = r#"{
        "name": "acme/test",
        "require": {},
        "scripts": {"post-install-cmd": "echo hi"}
    }"#;
    let hash = hash_for(composer_json);
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [],
            "packages-dev": []
        }}"#
    );
    write_project(&proj, composer_json, &lock);

    let err = install_from_lock(&paths, &proj, InstallOptions::default())
        .expect_err("must reject scripts");
    let msg = format!("{err:#}");
    assert!(msg.contains("scripts"), "{msg}");
}

#[test]
fn preflight_reports_all_failures_together() {
    // Plugin + tar + scripts — every reject path firing at once.
    // The aggregated error must mention every one.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    let composer_json = r#"{
        "name": "acme/big",
        "require": {},
        "scripts": {"pre-autoload-dump": "echo"}
    }"#;
    let hash = hash_for(composer_json);
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [
                {{
                    "name": "acme/plugin",
                    "version": "1.0.0",
                    "type": "composer-plugin",
                    "dist": {{
                        "type": "zip",
                        "url": "https://example/p.zip",
                        "shasum": "1111111111111111111111111111111111111111"
                    }}
                }},
                {{
                    "name": "acme/tar",
                    "version": "1.0.0",
                    "dist": {{
                        "type": "tar",
                        "url": "https://example/t.tar.gz",
                        "shasum": "2222222222222222222222222222222222222222"
                    }}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project(&proj, composer_json, &lock);

    let err = install_from_lock(&paths, &proj, InstallOptions::default())
        .expect_err("must reject");
    let msg = format!("{err:#}");
    assert!(msg.contains("scripts"), "scripts: {msg}");
    assert!(msg.contains("acme/plugin"), "plugin: {msg}");
    assert!(msg.contains("acme/tar"), "tar: {msg}");
}

#[test]
fn no_dev_skips_dev_only_packages_in_preflight() {
    // A composer-plugin in packages-dev would normally trip preflight,
    // but with --no-dev it's invisible to the install.
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    let hash = hash_for(MINIMAL_COMPOSER_JSON);
    let lock = format!(
        r#"{{
            "content-hash": "{hash}",
            "packages": [],
            "packages-dev": [
                {{
                    "name": "acme/dev-plugin",
                    "version": "1.0.0",
                    "type": "composer-plugin",
                    "dist": {{
                        "type": "zip",
                        "url": "https://example/p.zip",
                        "shasum": "1111111111111111111111111111111111111111"
                    }}
                }}
            ]
        }}"#
    );
    write_project(&proj, MINIMAL_COMPOSER_JSON, &lock);

    // With dev included, this would error. With --no-dev it gets past
    // preflight; the install then proceeds to actually do work
    // (autoload dump etc.), which succeeds against an empty package set.
    let summary = install_from_lock(
        &paths,
        &proj,
        InstallOptions { no_dev: true },
    )
    .expect("preflight should pass with --no-dev");
    assert_eq!(summary.packages_installed, 0);
    assert_eq!(summary.packages_already_present, 0);
    assert!(summary.no_dev);
}

#[test]
fn install_attaches_per_host_auth_to_dist_download() {
    // Real-world Magento-style topology: dist URLs live on the same
    // host as the (auth-gated) metadata. The orchestrator must
    // recognise the URL's host, look up `config.http-basic.<host>`
    // from composer.json (or `auth.json`), and send the
    // `Authorization` header on each ZIP download. Without this
    // wiring, `bougie composer install` 401s on every private dist.
    use sha1::Digest as _;
    use std::io::Write as _;
    use wiremock::matchers::{header, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        let digest = sha1::Sha1::digest(bytes);
        let mut s = String::with_capacity(40);
        for b in digest {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    // Minimal zip with a `composer.json` at the top level. The
    // orchestrator's preflight wants the dist sha1 to round-trip; we
    // hash the bytes we serve.
    fn build_zip(top: &str) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file(format!("{top}/composer.json"), opts).unwrap();
        zw.write_all(br#"{"name":"acme/foo"}"#).unwrap();
        zw.finish().unwrap();
        buf
    }

    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();

    let zip_body = build_zip("acme-foo-abc");
    let zip_sha1 = sha1_hex(&zip_body);

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        // Authenticated path returns the ZIP; unauthenticated
        // fall-through returns 401, fails the test.
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .and(header("Authorization", "Basic dXNlcjpwYXNz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_body))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-foo.zip"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    // composer.json declares per-host basic auth for the mock's host
    // (everything after `http://`, port included would be the
    // full authority; we strip the port to match host-only).
    let host = uri.trim_start_matches("http://").split(':').next().unwrap();
    let composer_json = format!(
        r#"{{
            "name": "acme/test",
            "require": {{"acme/foo": "1.0.0"}},
            "config": {{
                "http-basic": {{
                    "{host}": {{"username": "user", "password": "pass"}}
                }}
            }}
        }}"#
    );
    let content_hash = hash_for(&composer_json);
    let lock = format!(
        r#"{{
            "content-hash": "{content_hash}",
            "packages": [
                {{
                    "name": "acme/foo",
                    "version": "1.0.0",
                    "type": "library",
                    "dist": {{
                        "type": "zip",
                        "url": "{uri}/dists/acme-foo.zip",
                        "shasum": "{zip_sha1}"
                    }}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project(&proj, &composer_json, &lock);

    let summary = install_from_lock(&paths, &proj, InstallOptions::default())
        .expect("install must succeed with the auth header attached");
    assert_eq!(summary.packages_installed, 1);
    assert!(
        proj.join("vendor/acme/foo/composer.json").is_file(),
        "package must be extracted into vendor/",
    );
}
