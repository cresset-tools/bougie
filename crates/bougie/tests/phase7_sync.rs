//! Phase 7: end-to-end `bougie sync` against a wiremock fake index.

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

fn build_extension_tarball() -> (Vec<u8>, String) {
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        let body = b"fake extension";
        let mut header = tar::Header::new_gnu();
        header.set_path("lib/extensions/redis.so").unwrap();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, &body[..]).unwrap();
        builder.finish().unwrap();
    }
    let zst = zstd::stream::encode_all(&tar_buf[..], 0).unwrap();
    let hash = hex(&zst);
    (zst, hash)
}

struct Fixture {
    server: MockServer,
    pub_pem: String,
    snapshot: String,
    target: String,
    manifest_sha: String,
    blob_sha: String,
    redis_manifest_sha: String,
    redis_blob_sha: String,
}

async fn build_fixture() -> Fixture {
    let keys = ECDSAKeys::new(EllipticCurve::P256).unwrap();
    let pub_pem = keys.as_inner().public_key_to_pem().unwrap();
    let signer = keys.to_sigstore_signer().unwrap();

    let server = MockServer::start().await;
    let target = bougie::Triple::detect().unwrap().to_string();

    let (blob_bytes, blob_sha) = build_php_tarball();
    let blob_url = format!("{}/blobs/{blob_sha}", server.uri());

    let manifest_path_abs =
        format!("/targets/{target}/manifests/php/8.3/php-8.3.12-{target}-nts.json");
    let manifest_json = serde_json::json!({
        "schema": 1,
        "kind": "interpreter",
        "name": "php",
        "tag": format!("php-8.3.12-{target}-nts"),
        "version": "8.3.12",
        "target": target,
        "flavor": "nts",
        "abi": {"php":"8.3","zend_module_api_no":"20230831","zend_extension_api_no":"420230831"},
        "libc": {"family":"gnu","min":"2.17"},
        "blob": {"url": blob_url, "sha256": blob_sha},
        "closure": [],
        "sapis": ["cli","fpm"]
    });
    let manifest_bytes = serde_json::to_vec(&manifest_json).unwrap();
    let manifest_sha = hex(&manifest_bytes);

    let section_json = serde_json::json!({
        "schema": 1, "name": "php", "kind": "interpreter",
        "target": target,
        "artifacts": [{
            "tag": format!("php-8.3.12-{target}-nts"),
            "version": "8.3.12",
            "flavor": "nts",
            "manifest": {"path": manifest_path_abs, "sha256": manifest_sha},
            "yanked": false, "frozen": false
        }]
    });
    let section_bytes = serde_json::to_vec(&section_json).unwrap();
    let section_sha = hex(&section_bytes);

    let (redis_blob_bytes, redis_blob_sha) = build_extension_tarball();
    let redis_blob_url = format!("{}/blobs/{redis_blob_sha}", server.uri());
    let redis_manifest_path_abs =
        format!("/targets/{target}/manifests/ext/redis/6.0.0/redis-6.0.0+php83-{target}-nts.json");
    let redis_manifest_json = serde_json::json!({
        "schema": 1,
        "kind": "extension",
        "name": "redis",
        "tag": format!("redis-6.0.0+php83-{target}-nts"),
        "version": "6.0.0",
        "target": target,
        "flavor": "nts",
        "abi": {"php":"8.3","zend_module_api_no":"20230831","zend_extension_api_no":"420230831"},
        "libc": {"family":"gnu","min":"2.17"},
        "blob": {"url": redis_blob_url, "sha256": redis_blob_sha},
        "extension": {"path":"lib/extensions/redis.so","sha256":"unused"},
        "closure": []
    });
    let redis_manifest_bytes = serde_json::to_vec(&redis_manifest_json).unwrap();
    let redis_manifest_sha = hex(&redis_manifest_bytes);
    let redis_section_json = serde_json::json!({
        "schema": 1, "name": "redis", "kind": "extension",
        "target": target,
        "artifacts": [{
            "tag": format!("redis-6.0.0+php83-{target}-nts"),
            "version": "6.0.0",
            "flavor": "nts",
            "php_minor": "8.3",
            "manifest": {"path": redis_manifest_path_abs, "sha256": redis_manifest_sha},
            "yanked": false, "frozen": false
        }]
    });
    let redis_section_bytes = serde_json::to_vec(&redis_section_json).unwrap();
    let redis_section_sha = hex(&redis_section_bytes);

    let publish_version = "20260509T000000Z";
    let root = serde_json::json!({
        "schema": 1, "version": publish_version, "generated": "2026-05-09T00:00:00Z",
        "targets": {
            target.clone(): {
                "sections": {
                    "interpreter/php": {"sha256": section_sha, "size": section_bytes.len()},
                    "extension/redis": {"sha256": redis_section_sha, "size": redis_section_bytes.len()}
                }
            }
        }
    });
    let root_bytes = serde_json::to_vec(&root).unwrap();
    let sig = signer.sign(&root_bytes).unwrap();
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(&sig);

    let section_path =
        format!("/versions/{publish_version}/targets/{target}/sections/interpreter/php.json");
    let manifest_path = manifest_path_abs.clone();
    let blob_path = format!("/blobs/{blob_sha}");
    let redis_section_path =
        format!("/versions/{publish_version}/targets/{target}/sections/extension/redis.json");
    let redis_blob_path = format!("/blobs/{redis_blob_sha}");

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
        .and(path(section_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(section_bytes))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(manifest_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(manifest_bytes))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(blob_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(blob_bytes))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(redis_section_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(redis_section_bytes))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(redis_manifest_path_abs))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(redis_manifest_bytes))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(redis_blob_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(redis_blob_bytes))
        .mount(&server)
        .await;

    Fixture {
        server,
        pub_pem,
        snapshot: publish_version.into(),
        target,
        manifest_sha,
        blob_sha,
        redis_manifest_sha,
        redis_blob_sha,
    }
}

fn write_trust_root(env: &TestEnv, pem: &str) -> std::path::PathBuf {
    let p = env.home_path().join("test-trust-root.pub");
    std::fs::write(&p, pem).unwrap();
    p
}

#[test]
fn sync_installs_php_and_writes_state() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();
    let trust = write_trust_root(&env, &fx.pub_pem);
    let proj = tempfile::TempDir::new().unwrap();

    // Init the project (composer.json with require.php = "8.3.12").
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"require":{"php":"8.3.12"}}"#,
    )
    .unwrap();

    env.bougie()
        .current_dir(proj.path())
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust)
        .arg("--verbose")
        .arg("sync")
        .assert()
        .success()
        // The two-line uv-style summary is always present; the
        // interpreter detail is `--verbose`-only.
        .stdout(contains("Resolved"))
        .stdout(contains("php 8.3.12-nts"));

    let resolved = proj.path().join("vendor/bougie/state/resolved");
    assert!(resolved.is_file());
    let body = std::fs::read_to_string(&resolved).unwrap();
    assert_eq!(body.trim(), "8.3.12-nts");

    let php_link = proj.path().join("vendor/bougie/bin/php");
    assert!(php_link.symlink_metadata().is_ok());
    // The `composer` shim is still seeded — it now routes to bougie's
    // native Composer subcommands rather than a phar.
    let composer_link = proj.path().join("vendor/bougie/bin/composer");
    assert!(composer_link.symlink_metadata().is_ok());
}

fn disable_baseline(project: &std::path::Path) {
    let mut config = String::from("[extensions]\n");
    for name in bougie_installer::baseline::BASELINE_EXTENSIONS {
        use std::fmt::Write as _;
        let _ = writeln!(config, "{name} = false");
    }
    std::fs::write(project.join("bougie.toml"), config).unwrap();
}

fn write_toolchain_lock(project: &std::path::Path, fx: &Fixture, blob_sha: String) {
    use bougie_lock::{ArtifactDigest, PhpPin, TargetArtifacts, ToolchainLock};
    use std::collections::BTreeMap;
    ToolchainLock {
        version: bougie_lock::FORMAT_VERSION,
        snapshot: fx.snapshot.clone(),
        php: PhpPin {
            constraint: "8.3.12".into(),
            version: "8.3.12".into(),
            flavor: "nts".into(),
        },
        extensions: BTreeMap::new(),
        services: BTreeMap::new(),
        targets: BTreeMap::from([(
            fx.target.clone(),
            TargetArtifacts {
                php: ArtifactDigest {
                    manifest_sha256: fx.manifest_sha.clone(),
                    blob_sha256: blob_sha,
                },
                extensions: BTreeMap::new(),
                services: BTreeMap::new(),
            },
        )]),
    }
    .write(project)
    .unwrap();
}

#[test]
fn sync_installs_php_from_fresh_toolchain_lock() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();
    let trust = write_trust_root(&env, &fx.pub_pem);
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"require":{"php":"8.3.12"}}"#,
    )
    .unwrap();
    disable_baseline(proj.path());
    write_toolchain_lock(proj.path(), &fx, fx.blob_sha.clone());

    env.bougie()
        .current_dir(proj.path())
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust)
        .arg("sync")
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(proj.path().join("vendor/bougie/state/resolved"))
            .unwrap()
            .trim(),
        "8.3.12-nts"
    );

    let requests_before = runtime
        .block_on(fx.server.received_requests())
        .unwrap()
        .len();
    std::fs::remove_dir_all(proj.path().join("vendor")).unwrap();
    env.bougie()
        .current_dir(proj.path())
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust)
        .args(["sync", "--offline"])
        .assert()
        .success();
    let requests_after = runtime
        .block_on(fx.server.received_requests())
        .unwrap()
        .len();
    assert_eq!(
        requests_after, requests_before,
        "locked offline sync must use the verified index cache"
    );
}

#[test]
fn sync_rejects_blob_digest_drift_from_toolchain_lock() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();
    let trust = write_trust_root(&env, &fx.pub_pem);
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"require":{"php":"8.3.12"}}"#,
    )
    .unwrap();
    disable_baseline(proj.path());
    write_toolchain_lock(proj.path(), &fx, "0".repeat(64));

    env.bougie()
        .current_dir(proj.path())
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust)
        .arg("sync")
        .assert()
        .failure()
        .stderr(contains("blob sha256 mismatch"));
    assert!(!proj.path().join("vendor/bougie/state/resolved").exists());
}

#[test]
fn stale_toolchain_lock_warns_and_falls_back_to_floating_resolution() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();
    let trust = write_trust_root(&env, &fx.pub_pem);
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"require":{"php":"^8.3"}}"#,
    )
    .unwrap();
    disable_baseline(proj.path());
    write_toolchain_lock(proj.path(), &fx, fx.blob_sha.clone());

    env.bougie()
        .current_dir(proj.path())
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust)
        .arg("sync")
        .assert()
        .success()
        .stderr(contains("bougie.lock is stale: php constraint changed"));
}

#[test]
fn sync_lazily_adds_missing_current_target_to_lock() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();
    let trust = write_trust_root(&env, &fx.pub_pem);
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"require":{"php":"8.3.12"}}"#,
    )
    .unwrap();
    disable_baseline(proj.path());
    write_toolchain_lock(proj.path(), &fx, fx.blob_sha.clone());
    let mut lock = bougie_lock::ToolchainLock::read(proj.path())
        .unwrap()
        .unwrap();
    lock.targets.clear();
    lock.write(proj.path()).unwrap();

    env.bougie()
        .current_dir(proj.path())
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust)
        .arg("sync")
        .assert()
        .success()
        .stderr(contains("added current target"));

    let lock = bougie_lock::ToolchainLock::read(proj.path())
        .unwrap()
        .unwrap();
    assert!(lock.targets.contains_key(&fx.target));
}

#[test]
fn sync_installs_exact_locked_extension_with_verified_digests() {
    let runtime = rt();
    let fx = runtime.block_on(build_fixture());
    let env = TestEnv::new();
    let trust = write_trust_root(&env, &fx.pub_pem);
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"require":{"php":"8.3.12","ext-redis":"6.0.0"}}"#,
    )
    .unwrap();
    disable_baseline(proj.path());
    write_toolchain_lock(proj.path(), &fx, fx.blob_sha.clone());
    let mut lock = bougie_lock::ToolchainLock::read(proj.path())
        .unwrap()
        .unwrap();
    lock.extensions.insert(
        "redis".into(),
        bougie_lock::ExtensionPin {
            constraint: "6.0.0".into(),
            version: "6.0.0".into(),
            origin: bougie_lock::ExtensionOrigin::Declared,
        },
    );
    lock.targets.get_mut(&fx.target).unwrap().extensions.insert(
        "redis".into(),
        bougie_lock::ArtifactDigest {
            manifest_sha256: fx.redis_manifest_sha.clone(),
            blob_sha256: fx.redis_blob_sha.clone(),
        },
    );
    lock.write(proj.path()).unwrap();

    env.bougie()
        .current_dir(proj.path())
        .env("BOUGIE_INDEX_URL", fx.server.uri())
        .env("BOUGIE_TRUST_ROOT_PATH", &trust)
        .arg("sync")
        .assert()
        .success();

    let fragment = proj.path().join("vendor/bougie/conf.d/20-redis.ini");
    assert!(fragment.is_file());
    assert!(
        std::fs::read_to_string(fragment)
            .unwrap()
            .contains("ext-redis-6.0.0+php83-nts-")
    );
}

#[test]
fn sync_dry_run_changes_nothing() {
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(
        proj.path().join("composer.json"),
        r#"{"require":{"php":"8.3.12"}}"#,
    )
    .unwrap();

    env.bougie()
        .current_dir(proj.path())
        .args(["sync", "--dry-run"])
        .assert()
        .success();

    assert!(!proj.path().join("vendor/bougie/state/resolved").exists());
    assert!(!proj.path().join("vendor/bougie/bin").exists());
}

#[test]
fn sync_without_require_php_errors() {
    let env = TestEnv::new();
    let proj = tempfile::TempDir::new().unwrap();
    std::fs::write(proj.path().join("composer.json"), r"{}").unwrap();

    env.bougie()
        .current_dir(proj.path())
        .arg("sync")
        .assert()
        .failure()
        .stderr(contains("no PHP version constraint"));
}
