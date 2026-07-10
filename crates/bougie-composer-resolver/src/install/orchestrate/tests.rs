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
/// fixture lockfile carries the right value and `verify_content_hash`
/// passes (letting preflight be the part the test actually exercises).
fn hash_for(composer_json: &str) -> String {
    bougie_composer::lockfile::content_hash(composer_json.as_bytes()).unwrap()
}

const MINIMAL_COMPOSER_JSON: &str = r#"{
    "name": "acme/test",
    "require": {}
}"#;

#[test]
fn content_hash_mismatch_warns_but_installs() {
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

    // A stale lock no longer blocks install — it produces a warning and
    // installs the locked (empty) package set, matching Composer.
    let summary = install_from_lock(&paths, &proj, InstallOptions::default(), None)
        .expect("stale lock must warn, not error");
    let warning = summary
        .warnings
        .iter()
        .find(|w| w.contains("out of sync"))
        .unwrap_or_else(|| panic!("expected a content-hash warning, got {:?}", summary.warnings));
    assert!(warning.contains("composer update"), "{warning}");
}

#[test]
fn missing_composer_lock_errors_with_helpful_message() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("composer.json"), MINIMAL_COMPOSER_JSON).unwrap();

    let err = install_from_lock(&paths, &proj, InstallOptions::default(), None)
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

    let err = install_from_lock(&paths, &proj, InstallOptions::default(), None)
        .expect_err("must error when composer.json is missing");
    let msg = format!("{err:#}");
    assert!(msg.contains("not a Composer project"), "{msg}");
}

#[test]
fn composer_plugin_package_files_install_but_hooks_are_skipped() {
    // A composer-plugin package installs like any other — plugins can
    // double as runtime libraries (php-http/discovery's classes are
    // called by opensearch-php at runtime), so skipping the files broke
    // consumers. Only the plugin's install-time hooks never run, which
    // the install surfaces as a warning + `plugin_hooks_skipped`.
    use sha1::Digest as _;
    use std::io::Write as _;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();

    // A zip whose top-level dir carries a src/ file — the "runtime
    // library half" of a dual-purpose plugin.
    let mut zip_body: Vec<u8> = Vec::new();
    {
        let cursor = std::io::Cursor::new(&mut zip_body);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("acme-plugin-abc/composer.json", opts).unwrap();
        zw.write_all(br#"{"name":"acme/plugin","type":"composer-plugin"}"#)
            .unwrap();
        zw.start_file("acme-plugin-abc/src/Discovery.php", opts).unwrap();
        zw.write_all(b"<?php class Discovery {}").unwrap();
        zw.finish().unwrap();
    }
    let digest = sha1::Sha1::digest(&zip_body);
    let mut zip_sha1 = String::with_capacity(40);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(zip_sha1, "{b:02x}");
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/dists/acme-plugin.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

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
                        "url": "{uri}/dists/acme-plugin.zip",
                        "shasum": "{zip_sha1}"
                    }}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project(&proj, MINIMAL_COMPOSER_JSON, &lock);

    let summary = install_from_lock(&paths, &proj, InstallOptions::default(), None)
        .expect("install must succeed; only the plugin's hooks are skipped");
    assert_eq!(summary.packages_installed, 1);
    assert_eq!(summary.plugin_hooks_skipped, 1);
    assert_eq!(summary.warnings.len(), 1, "{:?}", summary.warnings);
    let warning = &summary.warnings[0];
    assert!(warning.contains("acme/plugin"), "{warning}");
    assert!(warning.contains("install-time hooks"), "{warning}");
    // The runtime half of the package must be on disk.
    assert!(
        proj.join("vendor/acme/plugin/src/Discovery.php").is_file(),
        "plugin package files must be extracted into vendor/",
    );
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

    let err = install_from_lock(&paths, &proj, InstallOptions::default(), None)
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

    let err = install_from_lock(&paths, &proj, InstallOptions::default(), None)
        .expect_err("must reject tar dist");
    let msg = format!("{err:#}");
    assert!(msg.contains("acme/tar"), "{msg}");
    assert!(msg.contains("`tar`"), "{msg}");
    assert!(msg.contains("zip dists"), "{msg}");
}

#[test]
fn composer_json_with_scripts_warns() {
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

    let summary = install_from_lock(&paths, &proj, InstallOptions::default(), None)
        .expect("install must succeed; scripts produce a warning, not an error");
    assert_eq!(summary.warnings.len(), 1, "{:?}", summary.warnings);
    assert!(summary.warnings[0].contains("scripts"), "{:?}", summary.warnings);
}

#[test]
fn preflight_reports_all_hard_blockers_together() {
    // Hard blockers (tar + source-only) coexist with soft warnings
    // (plugin + scripts). The error must aggregate every hard blocker;
    // the soft warnings are eaten by the hard-fail path but the
    // important thing is that the user gets the full list of blockers
    // in one go.
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
                }},
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
    write_project(&proj, composer_json, &lock);

    let err = install_from_lock(&paths, &proj, InstallOptions::default(), None)
        .expect_err("hard blockers must still fail install");
    let msg = format!("{err:#}");
    assert!(msg.contains("acme/tar"), "tar: {msg}");
    assert!(msg.contains("acme/sourceonly"), "sourceonly: {msg}");
}

#[test]
fn no_dev_hides_dev_only_packages_from_preflight() {
    // With --no-dev, the dev-only plugin is filtered out before
    // preflight even sees it — so no warning is emitted at all.
    // (Without --no-dev the same lockfile would emit a warning about
    // the plugin and install zero packages.)
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

    let summary = install_from_lock(
        &paths,
        &proj,
        InstallOptions { no_dev: true },
        None,
    )
    .expect("preflight should pass with --no-dev");
    assert_eq!(summary.packages_installed, 0);
    assert_eq!(summary.packages_already_present, 0);
    assert_eq!(summary.plugin_hooks_skipped, 0);
    assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);
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

    // composer.json declares per-origin basic auth for the mock's
    // authority (everything after `http://`, port included) — Composer
    // keys credentials by origin, so the `:PORT` is part of the key.
    let host = uri.trim_start_matches("http://");
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

    let summary = install_from_lock(&paths, &proj, InstallOptions::default(), None)
        .expect("install must succeed with the auth header attached");
    assert_eq!(summary.packages_installed, 1);
    assert!(
        proj.join("vendor/acme/foo/composer.json").is_file(),
        "package must be extracted into vendor/",
    );
}

#[test]
fn install_accepts_empty_shasum_via_github_zipball_style_dist() {
    // Real-world shape: every GitHub-served dist on public Packagist
    // ships with `shasum: ""` because Composer's `GitHubDriver::getDist`
    // emits an empty shasum (the archive is server-generated and
    // Packagist never sees the bytes). Composer treats that as
    // skip-verify; bougie must match. The dist URL also lacks an
    // upstream `.shasum`-derived cache key, so the cache must fall
    // back to `dist.reference` (the git ref) for naming.
    use std::io::Write as _;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn build_zip(top: &str) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file(format!("{top}/composer.json"), opts).unwrap();
        zw.write_all(br#"{"name":"acme/zipball"}"#).unwrap();
        zw.finish().unwrap();
        buf
    }

    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();

    // The git ref that doubles as the cache key.
    let reference = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let zip_body = build_zip(&format!("acme-zipball-{}", &reference[..7]));

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path(format!("/repos/acme/zipball/zipball/{reference}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let composer_json = r#"{
        "name": "acme/consumer",
        "require": {"acme/zipball": "1.0.0"}
    }"#;
    let content_hash = hash_for(composer_json);
    let lock = format!(
        r#"{{
            "content-hash": "{content_hash}",
            "packages": [
                {{
                    "name": "acme/zipball",
                    "version": "1.0.0",
                    "type": "library",
                    "dist": {{
                        "type": "zip",
                        "url": "{uri}/repos/acme/zipball/zipball/{reference}",
                        "reference": "{reference}",
                        "shasum": ""
                    }}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project(&proj, composer_json, &lock);

    let summary = install_from_lock(&paths, &proj, InstallOptions::default(), None)
        .expect("install must succeed despite empty shasum");
    assert_eq!(summary.packages_installed, 1);
    assert!(
        proj.join("vendor/acme/zipball/composer.json").is_file(),
        "package must be extracted into vendor/ via reference-keyed cache",
    );
}

#[test]
fn second_install_skips_up_to_date_packages() {
    use std::io::Write as _;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn build_zip(top: &str) -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        let cursor = std::io::Cursor::new(&mut buf);
        let mut zw = zip::ZipWriter::new(cursor);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file(format!("{top}/composer.json"), opts).unwrap();
        zw.write_all(br#"{"name":"acme/bar"}"#).unwrap();
        zw.finish().unwrap();
        buf
    }

    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();

    let reference = "aabbccddaabbccddaabbccddaabbccddaabbccdd";
    let zip_body = build_zip(&format!("acme-bar-{}", &reference[..7]));

    let rt = rt();
    let (uri, _server) = rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path(format!("/repos/acme/bar/zipball/{reference}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_body))
            .mount(&server)
            .await;
        (server.uri(), server)
    });

    let composer_json = r#"{
        "name": "acme/consumer",
        "require": {"acme/bar": "2.0.0"}
    }"#;
    let content_hash = hash_for(composer_json);
    let lock = format!(
        r#"{{
            "content-hash": "{content_hash}",
            "packages": [
                {{
                    "name": "acme/bar",
                    "version": "2.0.0",
                    "type": "library",
                    "dist": {{
                        "type": "zip",
                        "url": "{uri}/repos/acme/bar/zipball/{reference}",
                        "reference": "{reference}",
                        "shasum": ""
                    }}
                }}
            ],
            "packages-dev": []
        }}"#
    );
    write_project(&proj, composer_json, &lock);

    // First install — downloads and extracts.
    let s1 = install_from_lock(&paths, &proj, InstallOptions::default(), None)
        .expect("first install must succeed");
    assert_eq!(s1.packages_installed, 1);
    assert_eq!(s1.packages_up_to_date, 0);
    assert!(proj.join("vendor/acme/bar/composer.json").is_file());
    assert!(proj.join("vendor/composer/installed.json").is_file());

    // Second install — same lock, same vendor. Should be fully
    // up-to-date with no downloads or extractions.
    let s2 = install_from_lock(&paths, &proj, InstallOptions::default(), None)
        .expect("second install must succeed");
    assert_eq!(s2.packages_installed, 0, "no fresh downloads");
    assert_eq!(s2.packages_already_present, 0, "no cache-only extractions");
    assert_eq!(s2.packages_up_to_date, 1, "package is up to date");
    assert!(
        proj.join("vendor/acme/bar/composer.json").is_file(),
        "vendor dir must still be intact",
    );
}

#[test]
fn diff_removes_stale_vendor_dirs() {
    use super::super::orchestrate::{diff_install_set, InstalledState};
    use bougie_composer::lockfile::LockPackage;

    let tmp = TempDir::new().unwrap();
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();

    // Simulate a previous install that had acme/foo + acme/stale.
    let stale_dir = proj.join("vendor/acme/stale");
    std::fs::create_dir_all(&stale_dir).unwrap();

    let mut old_packages = HashMap::new();
    old_packages.insert("acme/foo".into(), "ref1".into());
    old_packages.insert("acme/stale".into(), "ref2".into());
    let state = Some(InstalledState {
        packages: old_packages,
    });

    // Current lock only has acme/foo.
    let foo_lock: LockPackage = serde_json::from_value(serde_json::json!({
        "name": "acme/foo",
        "version": "1.0.0",
        "dist": {
            "type": "zip",
            "url": "https://example/foo.zip",
            "reference": "ref1",
            "shasum": "0000000000000000000000000000000000000000"
        }
    }))
    .unwrap();

    // acme/foo's vendor dir exists → up-to-date.
    let foo_vendor = proj.join("vendor/acme/foo");
    std::fs::create_dir_all(&foo_vendor).unwrap();

    let installable = vec![&foo_lock];
    let (need_install, up_to_date, removed) =
        diff_install_set(&installable, &state, &proj, &std::collections::HashSet::new());

    assert!(need_install.is_empty(), "acme/foo is up-to-date");
    assert_eq!(up_to_date, 1);
    assert_eq!(removed, 1, "acme/stale must be removed");
    assert!(!stale_dir.exists(), "stale vendor dir must be deleted");
}

#[test]
fn missing_vendor_dir_forces_reinstall() {
    use super::super::orchestrate::{diff_install_set, InstalledState};
    use bougie_composer::lockfile::LockPackage;

    let tmp = TempDir::new().unwrap();
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();

    let mut old_packages = HashMap::new();
    old_packages.insert("acme/foo".into(), "ref1".into());
    let state = Some(InstalledState {
        packages: old_packages,
    });

    let foo_lock: LockPackage = serde_json::from_value(serde_json::json!({
        "name": "acme/foo",
        "version": "1.0.0",
        "dist": {
            "type": "zip",
            "url": "https://example/foo.zip",
            "reference": "ref1",
            "shasum": "0000000000000000000000000000000000000000"
        }
    }))
    .unwrap();

    // Do NOT create the vendor dir — simulates manual deletion.
    let installable = vec![&foo_lock];
    let (need_install, up_to_date, removed) =
        diff_install_set(&installable, &state, &proj, &std::collections::HashSet::new());

    assert_eq!(need_install.len(), 1, "must re-install when vendor dir is missing");
    assert_eq!(need_install[0].name, "acme/foo");
    assert_eq!(up_to_date, 0);
    assert_eq!(removed, 0);
}

#[test]
fn changed_reference_forces_reinstall() {
    use super::super::orchestrate::{diff_install_set, InstalledState};
    use bougie_composer::lockfile::LockPackage;

    let tmp = TempDir::new().unwrap();
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();

    // Previous install had ref1.
    let mut old_packages = HashMap::new();
    old_packages.insert("acme/foo".into(), "old_ref".into());
    let state = Some(InstalledState {
        packages: old_packages,
    });

    // Lock now has a different reference (version bumped).
    let foo_lock: LockPackage = serde_json::from_value(serde_json::json!({
        "name": "acme/foo",
        "version": "2.0.0",
        "dist": {
            "type": "zip",
            "url": "https://example/foo.zip",
            "reference": "new_ref",
            "shasum": "0000000000000000000000000000000000000000"
        }
    }))
    .unwrap();

    // Vendor dir exists from the old version.
    std::fs::create_dir_all(proj.join("vendor/acme/foo")).unwrap();

    let installable = vec![&foo_lock];
    let (need_install, up_to_date, removed) =
        diff_install_set(&installable, &state, &proj, &std::collections::HashSet::new());

    assert_eq!(need_install.len(), 1, "must re-install when reference changed");
    assert_eq!(need_install[0].name, "acme/foo");
    assert_eq!(up_to_date, 0);
    assert_eq!(removed, 0);
}

// -------------------- opt-in root scripts (ScriptHooks) --------------------

use std::cell::RefCell;

/// Records the lifecycle events fired, in order, so tests can assert the
/// hook sequence without running real scripts.
#[derive(Default)]
struct RecordingHooks {
    events: RefCell<Vec<&'static str>>,
}

impl ScriptHooks for RecordingHooks {
    fn pre_cmd(&self) -> Result<()> {
        self.events.borrow_mut().push("pre_cmd");
        Ok(())
    }
    fn pre_autoload_dump(&self) -> Result<()> {
        self.events.borrow_mut().push("pre_autoload_dump");
        Ok(())
    }
    fn post_autoload_dump(&self) -> Result<()> {
        self.events.borrow_mut().push("post_autoload_dump");
        Ok(())
    }
    fn post_cmd(&self) -> Result<()> {
        self.events.borrow_mut().push("post_cmd");
        Ok(())
    }
}

const SCRIPTED_COMPOSER_JSON: &str = r#"{
    "name": "acme/test",
    "require": {},
    "scripts": {
        "post-install-cmd": ["echo hi"]
    }
}"#;

fn empty_lock_for(json: &str) -> String {
    format!(
        r#"{{
            "content-hash": "{}",
            "packages": [],
            "packages-dev": []
        }}"#,
        hash_for(json)
    )
}

#[test]
fn hooks_fire_in_lifecycle_order() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    write_project(&proj, MINIMAL_COMPOSER_JSON, &empty_lock_for(MINIMAL_COMPOSER_JSON));

    let hooks = RecordingHooks::default();
    install_from_lock(&paths, &proj, InstallOptions::default(), Some(&hooks))
        .expect("install with hooks must succeed");
    assert_eq!(
        hooks.events.into_inner(),
        vec!["pre_cmd", "pre_autoload_dump", "post_autoload_dump", "post_cmd"],
    );
}

#[test]
fn scripts_warning_suppressed_when_hooks_present() {
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    write_project(&proj, SCRIPTED_COMPOSER_JSON, &empty_lock_for(SCRIPTED_COMPOSER_JSON));

    // Scripts ON: hooks run them, so no "declares scripts" warning.
    let hooks = RecordingHooks::default();
    let on = install_from_lock(&paths, &proj, InstallOptions::default(), Some(&hooks)).unwrap();
    assert!(
        !on.warnings.iter().any(|w| w.contains("declares `scripts`")),
        "scripts-on must not warn: {:?}",
        on.warnings
    );

    // Scripts OFF: warning is present and advertises the opt-in.
    let off = install_from_lock(&paths, &proj, InstallOptions::default(), None).unwrap();
    let warning = off
        .warnings
        .iter()
        .find(|w| w.contains("declares `scripts`"))
        .unwrap_or_else(|| panic!("scripts-off must warn: {:?}", off.warnings));
    assert!(warning.contains("[scripts] run = true"), "{warning}");
    assert!(warning.contains("--scripts"), "{warning}");
}

#[test]
fn scripts_warning_suppressed_for_only_reproduced_scripts() {
    // A project whose only script is Laravel's standard discovery hook —
    // bougie reproduces it natively, so claiming "does not run them" would
    // be misleading. No laravel/framework in the lock here: we're exercising
    // the preflight suppression, not the discovery path.
    let json = r#"{
        "name": "acme/test",
        "require": {},
        "scripts": {
            "post-autoload-dump": ["@php artisan package:discover"]
        }
    }"#;
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    write_project(&proj, json, &empty_lock_for(json));

    let summary = install_from_lock(&paths, &proj, InstallOptions::default(), None).unwrap();
    assert!(
        !summary.warnings.iter().any(|w| w.contains("declares `scripts`")),
        "pure-discovery scripts must not warn: {:?}",
        summary.warnings
    );
}

#[test]
fn laravel_drift_guard_skipped_when_scripts_on() {
    // laravel/framework as a metapackage (no dist → not fetched) plus a
    // blocking post-autoload-dump (a custom step with no recognizable
    // default). Scripts OFF: the drift guard errors. Scripts ON: the guard
    // is skipped (the real post-autoload-dump runs via the hook).
    let json = r#"{
        "name": "acme/test",
        "require": {},
        "scripts": {
            "post-autoload-dump": ["@php artisan custom:thing"]
        }
    }"#;
    let lock = format!(
        r#"{{
            "content-hash": "{}",
            "packages": [
                {{
                    "name": "laravel/framework",
                    "version": "11.0.0",
                    "type": "metapackage"
                }}
            ],
            "packages-dev": []
        }}"#,
        hash_for(json)
    );
    let tmp = TempDir::new().unwrap();
    let paths = paths_in(tmp.path());
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    write_project(&proj, json, &lock);

    // OFF → drift guard fires.
    let err = install_from_lock(&paths, &proj, InstallOptions::default(), None)
        .expect_err("drift guard must block when scripts are off");
    assert!(format!("{err:#}").contains("post-autoload-dump"), "{err:#}");

    // ON → guard skipped, hook runs instead.
    let hooks = RecordingHooks::default();
    install_from_lock(&paths, &proj, InstallOptions::default(), Some(&hooks))
        .expect("scripts-on must skip the drift guard");
    assert!(hooks.events.into_inner().contains(&"post_autoload_dump"));
}
