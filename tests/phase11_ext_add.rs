//! Phase 11: end-to-end `bougie ext add <name>` against a wiremock
//! fake index, exercising the no-composer-subprocess flow.
//!
//! This is the regression test for the redis case from the user
//! report: `composer require ext-redis` would interactively prompt
//! "Package 'ext-redis' does not exist but is provided by 3 packages"
//! and then fail the platform check. The new flow installs the `.so`
//! itself, writes the conf.d fragment, and edits composer.json +
//! composer.lock directly. Composer never runs.

use base64::Engine;
use predicates::str::contains;
use sha2::{Digest, Sha256};
use sigstore::crypto::signing_key::ecdsa::{ECDSAKeys, EllipticCurve};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::TestEnv;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A tar.zst with a fake `bin/php` script — the implicit sync expects
/// to find one inside the install tree.
fn build_php_tarball() -> (Vec<u8>, String) {
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        let script = b"#!/bin/sh\necho fake-php\n";
        let mut header = tar::Header::new_gnu();
        header.set_path("bin/php").unwrap();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, &script[..]).unwrap();
        builder.finish().unwrap();
    }
    let zst = zstd::stream::encode_all(&tar_buf[..], 0).unwrap();
    let h = hex(&zst);
    (zst, h)
}

/// A tar.zst whose layout matches what an extension publisher would
/// produce: a `lib/extensions/<api>/<name>.so` plus a fake LICENSE.
/// Returns `(blob_bytes, blob_sha256, so_path_in_tar, so_sha256)`.
fn build_ext_tarball(name: &str) -> (Vec<u8>, String, String, String) {
    let so_bytes = b"\x7fELF fake redis shared object\n".to_vec();
    let so_sha = hex(&so_bytes);
    let so_path = format!("lib/extensions/20230831/{name}.so");

    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);

        let mut header = tar::Header::new_gnu();
        header.set_path(&so_path).unwrap();
        header.set_size(so_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, &so_bytes[..]).unwrap();

        let license = b"MIT-ish, fake.\n";
        let mut lh = tar::Header::new_gnu();
        lh.set_path("LICENSE").unwrap();
        lh.set_size(license.len() as u64);
        lh.set_mode(0o644);
        lh.set_cksum();
        builder.append(&lh, &license[..]).unwrap();

        builder.finish().unwrap();
    }
    let zst = zstd::stream::encode_all(&tar_buf[..], 0).unwrap();
    let blob_sha = hex(&zst);
    (zst, blob_sha, so_path, so_sha)
}

struct Fixture {
    server: MockServer,
    pub_pem: String,
    composer_server: MockServer,
    /// First 8 hex chars of the redis blob's sha256, used to derive
    /// the expected store dir.
    redis_blob_sha8: String,
    redis_so_path_in_tarball: String,
}

#[allow(clippy::too_many_lines)]
async fn build_fixture() -> Fixture {
    let keys = ECDSAKeys::new(EllipticCurve::P256).unwrap();
    let pub_pem = keys.as_inner().public_key_to_pem().unwrap();
    let signer = keys.to_sigstore_signer().unwrap();

    let server = MockServer::start().await;
    let target = bougie::Triple::detect().unwrap().to_string();

    // ---- PHP interpreter (for the implicit ensure_synced) ---------------
    let (php_blob, php_sha) = build_php_tarball();
    let php_blob_url = format!("{}/blobs/{php_sha}", server.uri());
    let php_manifest_path = format!(
        "/targets/{target}/manifests/php/8.3/php-8.3.12-{target}-nts.json"
    );
    let php_manifest_json = serde_json::json!({
        "schema": 1, "kind": "interpreter", "name": "php",
        "tag": format!("php-8.3.12-{target}-nts"),
        "version": "8.3.12", "target": target, "flavor": "nts",
        "abi": {"php":"8.3","zend_module_api_no":"20230831","zend_extension_api_no":"420230831"},
        "libc": {"family":"gnu","min":"2.17"},
        "blob": {"url": php_blob_url, "sha256": php_sha},
        "closure": [], "sapis": ["cli","fpm"]
    });
    let php_manifest_bytes = serde_json::to_vec(&php_manifest_json).unwrap();
    let php_manifest_sha = hex(&php_manifest_bytes);
    let php_section_json = serde_json::json!({
        "schema": 1, "name": "php", "kind": "interpreter", "target": target,
        "artifacts": [{
            "tag": format!("php-8.3.12-{target}-nts"),
            "version": "8.3.12", "flavor": "nts",
            "manifest": {"path": php_manifest_path, "sha256": php_manifest_sha},
            "yanked": false, "frozen": false
        }]
    });
    let php_section_bytes = serde_json::to_vec(&php_section_json).unwrap();
    let php_section_sha = hex(&php_section_bytes);

    // ---- redis extension ------------------------------------------------
    let (redis_blob, redis_blob_sha, so_path, so_sha) = build_ext_tarball("redis");
    let redis_blob_sha8: String = redis_blob_sha.chars().take(8).collect();
    let redis_blob_url = format!("{}/blobs/{redis_blob_sha}", server.uri());
    let redis_manifest_path = format!(
        "/targets/{target}/manifests/extension/redis/redis-6.0.2+php83-{target}-nts.json"
    );
    let redis_manifest_json = serde_json::json!({
        "schema": 1, "kind": "extension", "name": "redis",
        "tag": format!("redis-6.0.2+php83-{target}-nts"),
        "version": "6.0.2", "target": target, "flavor": "nts",
        "abi": {"php":"8.3","zend_module_api_no":"20230831","zend_extension_api_no":"420230831"},
        "libc": {"family":"gnu","min":"2.17"},
        "blob": {"url": redis_blob_url, "sha256": redis_blob_sha},
        "extension": {"path": so_path, "sha256": so_sha},
        "closure": []
    });
    let redis_manifest_bytes = serde_json::to_vec(&redis_manifest_json).unwrap();
    let redis_manifest_sha = hex(&redis_manifest_bytes);
    let redis_section_json = serde_json::json!({
        "schema": 1, "name": "redis", "kind": "extension", "target": target,
        "artifacts": [{
            "tag": format!("redis-6.0.2+php83-{target}-nts"),
            "version": "6.0.2", "flavor": "nts",
            "php_minor": "8.3",
            "manifest": {"path": redis_manifest_path, "sha256": redis_manifest_sha},
            "yanked": false, "frozen": false
        }]
    });
    let redis_section_bytes = serde_json::to_vec(&redis_section_json).unwrap();
    let redis_section_sha = hex(&redis_section_bytes);

    // ---- root index -----------------------------------------------------
    let publish_version = "20260512T000000Z";
    let root = serde_json::json!({
        "schema": 1, "version": publish_version, "generated": "2026-05-12T00:00:00Z",
        "targets": {
            target.clone(): {
                "sections": {
                    "interpreter/php": {
                        "sha256": php_section_sha,
                        "size": php_section_bytes.len(),
                    },
                    "extension/redis": {
                        "sha256": redis_section_sha,
                        "size": redis_section_bytes.len(),
                    }
                }
            }
        }
    });
    let root_bytes = serde_json::to_vec(&root).unwrap();
    let sig = signer.sign(&root_bytes).unwrap();
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig);

    let php_section_path =
        format!("/versions/{publish_version}/targets/{target}/sections/interpreter/php.json");
    let redis_section_path =
        format!("/versions/{publish_version}/targets/{target}/sections/extension/redis.json");

    Mock::given(method("GET"))
        .and(path("/index.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(root_bytes)
                .insert_header("etag", "\"v1\""),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/index.json.sig"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(sig_b64.into_bytes()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(php_section_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(php_section_bytes))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(php_manifest_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(php_manifest_bytes))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/blobs/{php_sha}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(php_blob))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(redis_section_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(redis_section_bytes))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(redis_manifest_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(redis_manifest_bytes))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/blobs/{redis_blob_sha}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(redis_blob))
        .mount(&server)
        .await;

    let composer_server = build_composer_mock().await;

    Fixture {
        server,
        pub_pem,
        composer_server,
        redis_blob_sha8,
        redis_so_path_in_tarball: so_path,
    }
}

async fn build_composer_mock() -> MockServer {
    let server = MockServer::start().await;
    let phar_bytes = b"#!/usr/bin/env php\n<?php echo 'fake composer';\n".to_vec();
    let phar_sha = hex(&phar_bytes);
    let channels = serde_json::json!({
        "stable": [{"version":"2.8.5","path":"/download/2.8.5/composer.phar","shasum": phar_sha}],
        "preview": []
    });
    Mock::given(method("GET"))
        .and(path("/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(serde_json::to_vec(&channels).unwrap()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/download/2.8.5/composer.phar"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(phar_bytes.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/download/2.8.5/composer.phar.sha256sum"))
        .respond_with(ResponseTemplate::new(200).set_body_string(phar_sha))
        .mount(&server)
        .await;
    server
}

fn write_trust_root(env: &TestEnv, pem: &str) -> std::path::PathBuf {
    let p = env.home_path().join("test-trust-root.pub");
    std::fs::write(&p, pem).unwrap();
    p
}

#[test]
fn ext_add_redis_installs_so_and_edits_composer_json_no_composer_subprocess() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();
    let trust = write_trust_root(&env, &fx.pub_pem);
    let proj = tempfile::TempDir::new().unwrap();

    // Fresh project with a composer.json pinned to 8.3.12 (so the
    // extension resolver gets a concrete php_minor without hand-holding).
    std::fs::create_dir_all(proj.path().join(".bougie")).unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{
    "name": "acme/widget-tool",
    "require": {
        "php": "8.3.12"
    }
}
"#,
    )
    .unwrap();

    env.bougie()
        .current_dir(proj.path())
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_COMPOSER_BASE_URL", fx.composer_server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust)
        .args(["ext", "add", "redis"])
        .assert()
        .success()
        .stdout(contains("add ext-redis"));

    // composer.json now requires ext-redis.
    let cj: serde_json::Value =
        serde_json::from_slice(&std::fs::read(proj.path().join("composer.json")).unwrap()).unwrap();
    assert_eq!(
        cj.get("require").unwrap().get("ext-redis").unwrap(),
        &serde_json::Value::String("*".into()),
        "composer.json should have require.ext-redis"
    );

    // conf.d fragment exists with the right load directive + path.
    let frag = proj.path().join(".bougie/conf.d/20-redis.ini");
    let body = std::fs::read_to_string(&frag).expect("20-redis.ini should exist");
    assert!(body.contains("extension="));
    assert!(!body.contains("zend_extension"), "redis is a regular ext, not zend");

    // The .so itself was extracted into the content-addressed store
    // at the path the conf.d fragment points to. We can't assert the
    // exact dirname without recomputing the blob sha8, but we can
    // assert (a) the path the fragment names exists, and (b) it's
    // under $BOUGIE_HOME/store/.
    let extension_line = body
        .lines()
        .find(|l| l.starts_with("extension="))
        .expect("extension= line");
    let so_path: std::path::PathBuf =
        extension_line.trim_start_matches("extension=").trim().into();
    assert!(
        so_path.exists(),
        "the .so the fragment names should exist on disk at {}",
        so_path.display()
    );
    assert!(
        so_path
            .to_string_lossy()
            .contains(&format!("ext-redis-6.0.2+php83-nts-{}", fx.redis_blob_sha8)),
        "store dirname should be content-addressed: {}",
        so_path.display()
    );
    assert!(
        so_path
            .to_string_lossy()
            .contains(&fx.redis_so_path_in_tarball),
        "so_path should resolve to the manifest-declared tarball path: {}",
        so_path.display()
    );

    // Verify the absence of any composer.phar subprocess artefact —
    // the composer phar that ensure_synced fetches is a fake stub
    // (not executable), so if `bougie ext add` had tried to `exec`
    // composer the test would have errored out earlier. Belt and
    // suspenders: assert that we did NOT invoke composer require by
    // checking composer.json's formatting hasn't been mangled.
    // Composer rewrites composer.json with its own JsonFile encoder;
    // a pure bougie edit preserves the original JSON layout.
    let cj_bytes = std::fs::read(proj.path().join("composer.json")).unwrap();
    let cj_str = std::str::from_utf8(&cj_bytes).unwrap();
    assert!(cj_str.starts_with("{\n    \"name\":"));
}

#[test]
fn ext_add_redis_updates_lockfile_content_hash_when_present() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();
    let trust = write_trust_root(&env, &fx.pub_pem);
    let proj = tempfile::TempDir::new().unwrap();

    let composer_json_body = r#"{
    "name": "acme/widget-tool",
    "require": {
        "php": "8.3.12"
    }
}
"#;
    std::fs::create_dir_all(proj.path().join(".bougie")).unwrap();
    std::fs::write(proj.path().join("composer.json"), composer_json_body).unwrap();

    // Pre-existing composer.lock whose content-hash matches the
    // starting composer.json. After `ext add redis` the hash MUST
    // update — that's what makes `composer install` accept the
    // result without complaining about lockfile staleness.
    let starting_hash =
        bougie::composer::lockfile::content_hash(composer_json_body.as_bytes()).unwrap();
    let lock_body = serde_json::json!({
        "_readme": ["Test lockfile"],
        "content-hash": starting_hash.clone(),
        "packages": [],
        "packages-dev": [],
        "platform": {"php": "8.3.12"},
        "platform-dev": [],
    });
    std::fs::write(
        proj.path().join("composer.lock"),
        serde_json::to_vec_pretty(&lock_body).unwrap(),
    )
    .unwrap();

    env.bougie()
        .current_dir(proj.path())
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_COMPOSER_BASE_URL", fx.composer_server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust)
        .args(["ext", "add", "redis"])
        .assert()
        .success();

    let lock: serde_json::Value =
        serde_json::from_slice(&std::fs::read(proj.path().join("composer.lock")).unwrap()).unwrap();

    // platform map got the mirror.
    assert_eq!(
        lock.get("platform").unwrap().get("ext-redis").unwrap(),
        &serde_json::Value::String("*".into())
    );

    // content-hash changed and matches what we'd compute from the
    // post-edit composer.json bytes — the self-consistency that
    // makes `composer install` accept the result.
    let new_hash = lock
        .get("content-hash")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(new_hash, starting_hash, "content-hash should have changed");
    let post_edit_json = std::fs::read(proj.path().join("composer.json")).unwrap();
    let recomputed = bougie::composer::lockfile::content_hash(&post_edit_json).unwrap();
    assert_eq!(
        new_hash, recomputed,
        "lockfile content-hash must equal content_hash(written composer.json)"
    );
}

#[test]
fn ext_list_only_available_marks_installed_rows() {
    // After `ext add redis`, `ext list --only-available` must still
    // include redis AND mark it as installed — that's the user-visible
    // "what have I got vs what's offered" view. Earlier behaviour
    // excluded installed rows entirely, forcing a separate
    // `--only-installed` invocation to find out.
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();
    let trust = write_trust_root(&env, &fx.pub_pem);
    let proj = tempfile::TempDir::new().unwrap();

    std::fs::create_dir_all(proj.path().join(".bougie")).unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{
    "name": "acme/widget-tool",
    "require": {
        "php": "8.3.12"
    }
}
"#,
    )
    .unwrap();

    // First add redis, then list.
    env.bougie()
        .current_dir(proj.path())
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_COMPOSER_BASE_URL", fx.composer_server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust)
        .args(["ext", "add", "redis"])
        .assert()
        .success();

    let assertion = env
        .bougie()
        .current_dir(proj.path())
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust)
        .args(["ext", "list", "--only-available", "--format", "json-v1"])
        .assert()
        .success();
    let stdout = String::from_utf8(assertion.get_output().stdout.clone()).unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let items = parsed.get("items").and_then(|v| v.as_array()).unwrap();
    let redis = items
        .iter()
        .find(|it| it.get("name").and_then(|v| v.as_str()) == Some("redis"))
        .expect("redis row should appear under --only-available");

    let status: Vec<&str> = redis
        .get("status")
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .filter_map(|s| s.as_str())
        .collect();
    assert!(
        status.contains(&"available"),
        "redis row should carry `available`: {status:?}"
    );
    assert!(
        status.contains(&"installed"),
        "redis row should ALSO carry `installed` after `ext add redis`: {status:?}"
    );
}
